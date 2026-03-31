use std::{
    collections::HashSet,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use evalexpr::{build_operator_tree, ContextWithMutableVariables, HashMapContext, Value};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tower_http::{cors::CorsLayer, services::ServeDir};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    uploads_dir: Arc<PathBuf>,
    admin_token: Arc<String>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
}

#[derive(Serialize, Deserialize)]
struct Question {
    id: i64,
    question_text: Option<String>,
    question_image_path: Option<String>,
    expected_answer: String,
    solution_text: Option<String>,
    solution_image_path: Option<String>,
    time_limit_minutes: i64,
    position: i64,
    created_at: String,
}

#[derive(Serialize, Deserialize)]
struct Attempt {
    id: i64,
    question_id: i64,
    submitted_answer: String,
    auto_match: Option<bool>,
    is_correct: Option<bool>,
    feedback: Option<String>,
    submitted_at: String,
    reviewed_at: Option<String>,
}

#[derive(Deserialize)]
struct SubmitAttemptBody {
    submitted_answer: String,
}

#[derive(Deserialize)]
struct ReviewAttemptBody {
    is_correct: bool,
    feedback: Option<String>,
    share_solution: Option<bool>,
    solution_text_to_show: Option<String>,
    solution_image_to_show: Option<String>,
}

#[derive(Serialize)]
struct ReviewAttemptResponse {
    attempt: Attempt,
    solution_text: Option<String>,
    solution_image_path: Option<String>,
}

#[derive(Serialize)]
struct CurrentQuestionResponse {
    done: bool,
    question: Option<Question>,
}

#[derive(Serialize)]
struct NextQuestionResponse {
    done: bool,
    message: String,
    next_question: Option<Question>,
}

#[derive(Serialize)]
struct SessionStartResponse {
    question_id: i64,
    started_at_unix: i64,
    expires_at_unix: i64,
    remaining_seconds: i64,
    timed_out: bool,
}

#[derive(Serialize)]
struct SessionStatusResponse {
    question_id: i64,
    started: bool,
    started_at_unix: Option<i64>,
    expires_at_unix: Option<i64>,
    remaining_seconds: i64,
    timed_out: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let port = env::var("APP_PORT").unwrap_or_else(|_| "8080".to_string());
    let db_path = env::var("APP_DB_PATH").unwrap_or_else(|_| "./data/app.db".to_string());
    let uploads_dir = env::var("APP_UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
    let admin_token = env::var("APP_ADMIN_TOKEN").unwrap_or_else(|_| "change-me-admin-token".to_string());

    std::fs::create_dir_all(parent_or_current(&db_path))
        .with_context(|| format!("failed to create db directory for {db_path}"))?;
    std::fs::create_dir_all(&uploads_dir)
        .with_context(|| format!("failed to create uploads dir {uploads_dir}"))?;

    let conn = Connection::open(&db_path).with_context(|| format!("failed to open db {db_path}"))?;
    init_db(&conn)?;

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        uploads_dir: Arc::new(PathBuf::from(uploads_dir.clone())),
        admin_token: Arc::new(admin_token),
    };

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/questions", post(create_question).get(list_questions))
        .route("/api/questions/current", get(get_current_question))
        .route("/api/questions/:id/session/start", post(start_question_session))
        .route("/api/questions/:id/session", get(get_question_session_status))
        .route("/api/questions/:id/attempts", post(submit_attempt).get(list_attempts_for_question))
        .route("/api/attempts/:id/review", post(review_attempt))
        .route("/api/questions/next", post(move_to_next_question))
        .route("/api/state/reset", post(reset_state))
        .nest_service("/uploads", ServeDir::new(&uploads_dir))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = SocketAddr::from_str(&format!("0.0.0.0:{port}"))?;
    println!("Rust backend running on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS questions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            question_text TEXT,
            question_image_path TEXT,
            expected_answer TEXT NOT NULL,
            solution_text TEXT,
            solution_image_path TEXT,
            time_limit_minutes INTEGER NOT NULL CHECK (time_limit_minutes BETWEEN 2 AND 10),
            position INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            CHECK (question_text IS NOT NULL OR question_image_path IS NOT NULL),
            CHECK (solution_text IS NOT NULL OR solution_image_path IS NOT NULL)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_questions_position ON questions(position);

        CREATE TABLE IF NOT EXISTS attempts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            question_id INTEGER NOT NULL,
            submitted_answer TEXT NOT NULL,
            auto_match INTEGER,
            is_correct INTEGER,
            feedback TEXT,
            submitted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            reviewed_at TEXT,
            FOREIGN KEY (question_id) REFERENCES questions(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_attempts_question_id ON attempts(question_id);

        CREATE TABLE IF NOT EXISTS question_sessions (
            question_id INTEGER PRIMARY KEY,
            started_at_unix INTEGER NOT NULL,
            expires_at_unix INTEGER NOT NULL,
            FOREIGN KEY (question_id) REFERENCES questions(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            current_question_id INTEGER,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (current_question_id) REFERENCES questions(id)
        );

        INSERT OR IGNORE INTO state (id, current_question_id) VALUES (1, NULL);
        "#,
    )?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(HealthResponse { ok: true }))
}

async fn create_question(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    match handle_create_question(state, &mut multipart).await {
        Ok(question) => (StatusCode::CREATED, Json(question)).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, err),
    }
}

async fn handle_create_question(state: AppState, multipart: &mut Multipart) -> Result<Question> {
    let mut question_text: Option<String> = None;
    let mut question_image_path: Option<String> = None;
    let mut expected_answer: Option<String> = None;
    let mut solution_text: Option<String> = None;
    let mut solution_image_path: Option<String> = None;
    let mut time_limit_minutes: Option<i64> = None;
    let mut position: Option<i64> = None;

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "question_text" => {
                let value = field.text().await?.trim().to_string();
                if !value.is_empty() {
                    question_text = Some(value);
                }
            }
            "expected_answer" => {
                let value = field.text().await?.trim().to_string();
                if !value.is_empty() {
                    expected_answer = Some(value);
                }
            }
            "solution_text" => {
                let value = field.text().await?.trim().to_string();
                if !value.is_empty() {
                    solution_text = Some(value);
                }
            }
            "time_limit_minutes" => {
                let value = field.text().await?.trim().to_string();
                let parsed = value.parse::<i64>()?;
                time_limit_minutes = Some(parsed);
            }
            "position" => {
                let value = field.text().await?.trim().to_string();
                if !value.is_empty() {
                    position = Some(value.parse::<i64>()?);
                }
            }
            "question_image" => {
                question_image_path = Some(save_upload(field, &state.uploads_dir).await?);
            }
            "solution_image" => {
                solution_image_path = Some(save_upload(field, &state.uploads_dir).await?);
            }
            _ => {}
        }
    }

    let expected_answer = expected_answer.ok_or_else(|| anyhow!("expected_answer is required"))?;
    let time_limit_minutes = time_limit_minutes.ok_or_else(|| anyhow!("time_limit_minutes is required"))?;
    if !(2..=10).contains(&time_limit_minutes) {
        return Err(anyhow!("time_limit_minutes must be between 2 and 10"));
    }

    if question_text.is_none() && question_image_path.is_none() {
        return Err(anyhow!("question_text or question_image is required"));
    }
    if solution_text.is_none() && solution_image_path.is_none() {
        return Err(anyhow!("solution_text or solution_image is required"));
    }

    let mut conn = state
        .db
        .lock()
        .map_err(|_| anyhow!("failed to lock db connection"))?;

    let final_position = if let Some(pos) = position {
        if pos <= 0 {
            return Err(anyhow!("position must be a positive integer"));
        }
        pos
    } else {
        let max_pos: Option<i64> = conn
            .query_row("SELECT MAX(position) FROM questions", [], |r| r.get(0))
            .optional()?
            .flatten();
        max_pos.unwrap_or(0) + 1
    };

    conn.execute(
        r#"
        INSERT INTO questions (
            question_text,
            question_image_path,
            expected_answer,
            solution_text,
            solution_image_path,
            time_limit_minutes,
            position
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
        params![
            question_text,
            question_image_path,
            expected_answer,
            solution_text,
            solution_image_path,
            time_limit_minutes,
            final_position
        ],
    )
    .context("failed to insert question")?;

    let id = conn.last_insert_rowid();
    ensure_current_question_selected(&conn)?;
    load_question_by_id(&conn, id)
}

async fn list_questions(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    match load_all_questions(&conn) {
        Ok(questions) => (StatusCode::OK, Json(questions)).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn get_current_question(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<CurrentQuestionResponse> {
        ensure_current_question_selected(&conn)?;
        let current_id: Option<i64> = conn
            .query_row("SELECT current_question_id FROM state WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .flatten();

        if let Some(id) = current_id {
            let q = load_question_by_id(&conn, id)?;
            Ok(CurrentQuestionResponse {
                done: false,
                question: Some(q),
            })
        } else {
            Ok(CurrentQuestionResponse {
                done: true,
                question: None,
            })
        }
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn submit_attempt(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    Json(body): Json<SubmitAttemptBody>,
) -> impl IntoResponse {
    let submitted = body.submitted_answer.trim().to_string();
    if submitted.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, anyhow!("submitted_answer is required"));
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<Attempt> {
        let expected_answer: String = conn
            .query_row(
                "SELECT expected_answer FROM questions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("question not found"))?;

        let auto_match = auto_match_answer(&expected_answer, &submitted);
        let auto_match_db = auto_match.map(bool_to_i64);

        conn.execute(
            r#"
            INSERT INTO attempts (question_id, submitted_answer, auto_match, is_correct)
            VALUES (?1, ?2, ?3, NULL)
            "#,
            params![id, submitted, auto_match_db],
        )
        .context("failed to insert attempt")?;

        let attempt_id = conn.last_insert_rowid();
        load_attempt_by_id(&conn, attempt_id)
    })();

    match result {
        Ok(attempt) => (StatusCode::CREATED, Json(attempt)).into_response(),
        Err(err) => {
            let code = if err.to_string().contains("question not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(code, err)
        }
    }
}

async fn start_question_session(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(err) = require_admin(&headers, &state) {
        return error_response(StatusCode::UNAUTHORIZED, err);
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<SessionStartResponse> {
        let minutes: i64 = conn
            .query_row(
                "SELECT time_limit_minutes FROM questions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("question not found"))?;

        let started_at_unix = chrono::Utc::now().timestamp();
        let expires_at_unix = started_at_unix + (minutes * 60);

        conn.execute(
            r#"
            INSERT INTO question_sessions (question_id, started_at_unix, expires_at_unix)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(question_id) DO UPDATE SET
                started_at_unix = excluded.started_at_unix,
                expires_at_unix = excluded.expires_at_unix
            "#,
            params![id, started_at_unix, expires_at_unix],
        )?;

        Ok(SessionStartResponse {
            question_id: id,
            started_at_unix,
            expires_at_unix,
            remaining_seconds: expires_at_unix - started_at_unix,
            timed_out: false,
        })
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => {
            let code = if err.to_string().contains("question not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(code, err)
        }
    }
}

async fn get_question_session_status(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<SessionStatusResponse> {
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM questions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        if exists.is_none() {
            return Err(anyhow!("question not found"));
        }

        let session: Option<(i64, i64)> = conn
            .query_row(
                "SELECT started_at_unix, expires_at_unix FROM question_sessions WHERE question_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if let Some((started_at_unix, expires_at_unix)) = session {
            let now = chrono::Utc::now().timestamp();
            let remaining_seconds = (expires_at_unix - now).max(0);
            let timed_out = now >= expires_at_unix;
            Ok(SessionStatusResponse {
                question_id: id,
                started: true,
                started_at_unix: Some(started_at_unix),
                expires_at_unix: Some(expires_at_unix),
                remaining_seconds,
                timed_out,
            })
        } else {
            Ok(SessionStatusResponse {
                question_id: id,
                started: false,
                started_at_unix: None,
                expires_at_unix: None,
                remaining_seconds: 0,
                timed_out: false,
            })
        }
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => {
            let code = if err.to_string().contains("question not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(code, err)
        }
    }
}

async fn list_attempts_for_question(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let mut stmt = match conn.prepare(
        r#"
        SELECT id, question_id, submitted_answer, auto_match, is_correct, feedback, submitted_at, reviewed_at
        FROM attempts
        WHERE question_id = ?1
        ORDER BY id DESC
        "#,
    ) {
        Ok(s) => s,
        Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.into()),
    };

    let rows = match stmt.query_map(params![id], |row| map_attempt_row(row)) {
        Ok(r) => r,
        Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.into()),
    };

    let mut attempts = Vec::new();
    for item in rows {
        match item {
            Ok(a) => attempts.push(a),
            Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, err.into()),
        }
    }

    (StatusCode::OK, Json(attempts)).into_response()
}

async fn review_attempt(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReviewAttemptBody>,
) -> impl IntoResponse {
    if let Err(err) = require_admin(&headers, &state) {
        return error_response(StatusCode::UNAUTHORIZED, err);
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<ReviewAttemptResponse> {
        let attempt: Attempt = load_attempt_by_id(&conn, id).context("attempt not found")?;

        conn.execute(
            r#"
            UPDATE attempts
            SET is_correct = ?1, feedback = ?2, reviewed_at = CURRENT_TIMESTAMP
            WHERE id = ?3
            "#,
            params![bool_to_i64(body.is_correct), body.feedback, id],
        )
        .context("failed to update attempt")?;

        let updated_attempt = load_attempt_by_id(&conn, id)?;

        let mut solution_text = None;
        let mut solution_image_path = None;

        if !body.is_correct && body.share_solution.unwrap_or(false) {
            let question = load_question_by_id(&conn, attempt.question_id)?;
            solution_text = body
                .solution_text_to_show
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or(question.solution_text);

            solution_image_path = body
                .solution_image_to_show
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or(question.solution_image_path);
        }

        Ok(ReviewAttemptResponse {
            attempt: updated_attempt,
            solution_text,
            solution_image_path,
        })
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => {
            let code = if err.to_string().contains("attempt not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(code, err)
        }
    }
}

async fn move_to_next_question(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = require_admin(&headers, &state) {
        return error_response(StatusCode::UNAUTHORIZED, err);
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<NextQuestionResponse> {
        ensure_current_question_selected(&conn)?;

        let current_id: Option<i64> = conn
            .query_row("SELECT current_question_id FROM state WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .flatten();

        let Some(current_id) = current_id else {
            return Ok(NextQuestionResponse {
                done: true,
                message: "no questions available".to_string(),
                next_question: None,
            });
        };

        let current = load_question_by_id(&conn, current_id)?;

        let next_id: Option<i64> = conn
            .query_row(
                r#"
                SELECT id
                FROM questions
                WHERE (position > ?1) OR (position = ?1 AND id > ?2)
                ORDER BY position ASC, id ASC
                LIMIT 1
                "#,
                params![current.position, current.id],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(next_id) = next_id {
            conn.execute(
                "UPDATE state SET current_question_id = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
                params![next_id],
            )?;
            let next_q = load_question_by_id(&conn, next_id)?;
            Ok(NextQuestionResponse {
                done: false,
                message: "moved to next question".to_string(),
                next_question: Some(next_q),
            })
        } else {
            conn.execute(
                "UPDATE state SET current_question_id = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
                [],
            )?;
            Ok(NextQuestionResponse {
                done: true,
                message: "test completed".to_string(),
                next_question: None,
            })
        }
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn reset_state(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(err) = require_admin(&headers, &state) {
        return error_response(StatusCode::UNAUTHORIZED, err);
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, anyhow!("failed to lock db")),
    };

    let result = (|| -> Result<CurrentQuestionResponse> {
        conn.execute(
            "UPDATE state SET current_question_id = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
            [],
        )?;
        ensure_current_question_selected(&conn)?;
        let current_id: Option<i64> = conn
            .query_row("SELECT current_question_id FROM state WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .flatten();

        if let Some(id) = current_id {
            let q = load_question_by_id(&conn, id)?;
            Ok(CurrentQuestionResponse {
                done: false,
                question: Some(q),
            })
        } else {
            Ok(CurrentQuestionResponse {
                done: true,
                question: None,
            })
        }
    })();

    match result {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

fn load_question_by_id(conn: &Connection, id: i64) -> Result<Question> {
    conn.query_row(
        r#"
        SELECT id, question_text, question_image_path, expected_answer, solution_text, solution_image_path,
               time_limit_minutes, position, created_at
        FROM questions
        WHERE id = ?1
        "#,
        params![id],
        |row| {
            Ok(Question {
                id: row.get(0)?,
                question_text: row.get(1)?,
                question_image_path: row.get(2)?,
                expected_answer: row.get(3)?,
                solution_text: row.get(4)?,
                solution_image_path: row.get(5)?,
                time_limit_minutes: row.get(6)?,
                position: row.get(7)?,
                created_at: row.get(8)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow!("question not found"))
}

fn load_all_questions(conn: &Connection) -> Result<Vec<Question>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, question_text, question_image_path, expected_answer, solution_text, solution_image_path,
               time_limit_minutes, position, created_at
        FROM questions
        ORDER BY position ASC, id ASC
        "#,
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(Question {
            id: row.get(0)?,
            question_text: row.get(1)?,
            question_image_path: row.get(2)?,
            expected_answer: row.get(3)?,
            solution_text: row.get(4)?,
            solution_image_path: row.get(5)?,
            time_limit_minutes: row.get(6)?,
            position: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut questions = Vec::new();
    for item in rows {
        questions.push(item?);
    }
    Ok(questions)
}

fn load_attempt_by_id(conn: &Connection, id: i64) -> Result<Attempt> {
    conn.query_row(
        r#"
        SELECT id, question_id, submitted_answer, auto_match, is_correct, feedback, submitted_at, reviewed_at
        FROM attempts
        WHERE id = ?1
        "#,
        params![id],
        map_attempt_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("attempt not found"))
}

fn map_attempt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Attempt> {
    let auto_match_raw: Option<i64> = row.get(3)?;
    let is_correct_raw: Option<i64> = row.get(4)?;

    Ok(Attempt {
        id: row.get(0)?,
        question_id: row.get(1)?,
        submitted_answer: row.get(2)?,
        auto_match: auto_match_raw.map(|v| v == 1),
        is_correct: is_correct_raw.map(|v| v == 1),
        feedback: row.get(5)?,
        submitted_at: row.get(6)?,
        reviewed_at: row.get(7)?,
    })
}

fn ensure_current_question_selected(conn: &Connection) -> Result<()> {
    let current_id: Option<i64> = conn
        .query_row("SELECT current_question_id FROM state WHERE id = 1", [], |r| r.get(0))
        .optional()?
        .flatten();

    if current_id.is_some() {
        return Ok(());
    }

    let first_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM questions ORDER BY position ASC, id ASC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(first_id) = first_id {
        conn.execute(
            "UPDATE state SET current_question_id = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
            params![first_id],
        )?;
    }

    Ok(())
}

fn auto_match_answer(expected: &str, submitted: &str) -> Option<bool> {
    let left = expected.trim();
    let right = submitted.trim();

    if left.is_empty() || right.is_empty() {
        return None;
    }

    if normalize_plain(left) == normalize_plain(right) {
        return Some(true);
    }

    if let (Ok(a), Ok(b)) = (left.parse::<f64>(), right.parse::<f64>()) {
        return Some((a - b).abs() <= 1e-9);
    }

    expression_equivalent(left, right)
}

fn expression_equivalent(expected: &str, submitted: &str) -> Option<bool> {
    let expected_expr = normalize_equation(expected);
    let submitted_expr = normalize_equation(submitted);

    let vars = extract_variables(&expected_expr, &submitted_expr);
    let samples = [-3.0_f64, -1.5, -0.5, 0.5, 1.0, 2.0, 3.0, 4.0];

    if vars.is_empty() {
        let a = evaluate_expression(&expected_expr, &[]).ok()?;
        let b = evaluate_expression(&submitted_expr, &[]).ok()?;
        return Some((a - b).abs() <= 1e-7);
    }

    for i in 0..6 {
        let mut assignments = Vec::with_capacity(vars.len());
        for (j, var) in vars.iter().enumerate() {
            let val = samples[(i + j) % samples.len()];
            assignments.push((var.as_str(), val));
        }

        let a = evaluate_expression(&expected_expr, &assignments).ok()?;
        let b = evaluate_expression(&submitted_expr, &assignments).ok()?;

        if !a.is_finite() || !b.is_finite() {
            return None;
        }

        if (a - b).abs() > 1e-6 {
            return Some(false);
        }
    }

    Some(true)
}

fn normalize_equation(input: &str) -> String {
    let cleaned = normalize_plain(input);
    let parts: Vec<&str> = cleaned.split('=').collect();
    if parts.len() == 2 {
        format!("({})-({})", parts[0], parts[1])
    } else {
        cleaned
    }
}

fn normalize_plain(input: &str) -> String {
    input
        .trim()
        .replace(' ', "")
        .replace('^', "**")
        .to_lowercase()
}

fn extract_variables(a: &str, b: &str) -> Vec<String> {
    let re = Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]*").expect("regex must compile");
    let reserved: HashSet<&str> = HashSet::from(["pi", "e", "true", "false"]);
    let mut set: HashSet<String> = HashSet::new();

    for expr in [a, b] {
        for cap in re.find_iter(expr) {
            let v = cap.as_str().to_lowercase();
            if !reserved.contains(v.as_str()) {
                set.insert(v);
            }
        }
    }

    let mut vars: Vec<String> = set.into_iter().collect();
    vars.sort();
    vars
}

fn evaluate_expression(expr: &str, vars: &[(&str, f64)]) -> Result<f64> {
    let tree = build_operator_tree(expr).map_err(|e| anyhow!(e.to_string()))?;
    let mut context = HashMapContext::new();

    for (name, value) in vars {
        context
            .set_value((*name).to_string(), Value::Float(*value))
            .map_err(|e| anyhow!(e.to_string()))?;
    }

    tree.eval_number_with_context(&context)
        .map_err(|e| anyhow!(e.to_string()))
}

async fn save_upload(field: axum::extract::multipart::Field<'_>, uploads_dir: &Path) -> Result<String> {
    let file_name = field
        .file_name()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("uploaded file must have a name"))?;

    let ext = Path::new(&file_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let allowed = ["png", "jpg", "jpeg", "webp"];
    if !allowed.contains(&ext.as_str()) {
        return Err(anyhow!("image must be png, jpg, jpeg or webp"));
    }

    let data = field.bytes().await?;
    if data.len() > 8 * 1024 * 1024 {
        return Err(anyhow!("image size must be <= 8MB"));
    }

    let generated = format!("{}.{}", Uuid::new_v4(), ext);
    let dest = uploads_dir.join(&generated);
    std::fs::write(&dest, &data).with_context(|| format!("failed to write upload to {}", dest.display()))?;

    Ok(format!("/uploads/{generated}"))
}

fn bool_to_i64(v: bool) -> i64 {
    if v {
        1
    } else {
        0
    }
}

fn parent_or_current(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn require_admin(headers: &HeaderMap, state: &AppState) -> Result<()> {
    let token = headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim())
        .unwrap_or("");

    if token.is_empty() {
        return Err(anyhow!("missing x-admin-token header"));
    }

    if token != state.admin_token.as_str() {
        return Err(anyhow!("invalid admin token"));
    }

    Ok(())
}

fn error_response(status: StatusCode, err: anyhow::Error) -> axum::response::Response {
    (
        status,
        Json(ApiError {
            error: err.to_string(),
        }),
    )
        .into_response()
}
