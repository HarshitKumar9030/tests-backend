# Math Test Backend (Rust + Docker)

One-shot backend for your full manual test flow:
- Add question with text and/or image
- Add solution with text and/or image
- Set timer per question (2 to 10 minutes)
- Student submits answer as value or expression
- Backend gives auto-match hint (value/expression equivalence)
- You manually review and mark correct/incorrect
- Start per-question countdown timer sessions
- Check remaining seconds and timeout flag
- If incorrect, you can share solution text/image
- You manually move to next question

## Run

With Docker:
```bash
docker compose up --build
```

Directly with Rust:
```bash
cargo run
```

Server default URL: http://localhost:8080

Admin routes require header:
- x-admin-token: your secret token

Set token with env var:
- APP_ADMIN_TOKEN

## Nginx Site Setup (test.shriju.me)

Use the provided Nginx configs:
- deploy/nginx/test.shriju.me.conf (HTTP only)
- deploy/nginx/test.shriju.me.ssl.conf (HTTP to HTTPS + TLS)

### 1) Start backend on VM

```bash
cd /path/to/backend
docker compose up -d --build
```

### 2) Enable basic HTTP reverse proxy

```bash
sudo cp deploy/nginx/test.shriju.me.conf /etc/nginx/sites-available/test.shriju.me
sudo ln -s /etc/nginx/sites-available/test.shriju.me /etc/nginx/sites-enabled/test.shriju.me
sudo nginx -t
sudo systemctl reload nginx
```

### 3) DNS requirement

Point `test.shriju.me` A record to your VM public IP.

### 4) Enable TLS (Let's Encrypt)

Install certbot once:

```bash
sudo apt update
sudo apt install -y certbot python3-certbot-nginx
```

Issue cert:

```bash
sudo certbot certonly --nginx -d test.shriju.me
```

Switch to SSL config:

```bash
sudo rm /etc/nginx/sites-enabled/test.shriju.me
sudo cp deploy/nginx/test.shriju.me.ssl.conf /etc/nginx/sites-available/test.shriju.me
sudo ln -s /etc/nginx/sites-available/test.shriju.me /etc/nginx/sites-enabled/test.shriju.me
sudo nginx -t
sudo systemctl reload nginx
```

Optional renew test:

```bash
sudo certbot renew --dry-run
```

## Endpoints

### Health
- GET /api/health

### Create question
- POST /api/questions
- Content-Type: multipart/form-data

Fields:
- question_text (optional if question_image is present)
- question_image (optional if question_text is present)
- expected_answer (required, value/expression)
- solution_text (optional if solution_image is present)
- solution_image (optional if solution_text is present)
- time_limit_minutes (required, 2-10)
- position (optional; auto assigned if omitted)

Example:
```bash
curl -X POST http://localhost:8080/api/questions \
  -F "question_text=Solve: 2*x + 4 = 10" \
  -F "expected_answer=x=3" \
  -F "solution_text=x=3" \
  -F "time_limit_minutes=3" \
  -F "question_image=@./question.png" \
  -F "solution_image=@./solution.png"
```

### List all questions
- GET /api/questions

### Get current question
- GET /api/questions/current

### Start question timer session (admin)
- POST /api/questions/:id/session/start
- Header: x-admin-token

Response:
- question_id
- started_at_unix
- expires_at_unix
- remaining_seconds
- timed_out

### Get question timer session status
- GET /api/questions/:id/session

Response:
- started
- remaining_seconds
- timed_out

### Submit attempt
- POST /api/questions/:id/attempts

Body:
```json
{
  "submitted_answer": "3"
}
```

Response includes `auto_match` as a hint:
- `true` means answer seems equivalent
- `false` means not equivalent
- `null` means expression could not be reliably evaluated

### List attempts for a question
- GET /api/questions/:id/attempts

### Manually review attempt
- POST /api/attempts/:id/review
- Header: x-admin-token

Body:
```json
{
  "is_correct": false,
  "feedback": "Sign mistake",
  "share_solution": true
}
```

Optional override fields when sharing solution:
- solution_text_to_show
- solution_image_to_show

### Move manually to next question
- POST /api/questions/next
- Header: x-admin-token

### Reset state to first question
- POST /api/state/reset
- Header: x-admin-token

## Storage
- SQLite: ./data/app.db
- Uploads: ./uploads

Both are mounted as Docker volumes in compose.
