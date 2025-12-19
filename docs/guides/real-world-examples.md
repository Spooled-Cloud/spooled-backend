# Real-world examples (beginner friendly)

This guide shows **5 very common, real-life** ways people can use Spooled Cloud **right now**, without extra “automation services”.

The examples use:
- the **REST API** (`curl`)
- a tiny **worker** (Node.js) that just prints jobs to your terminal

If you can copy/paste and change a few values, you can do this.

---

## What Spooled is (in 60 seconds)

- **Webhook**: an external service sends you an HTTP `POST` when something happens (payment completed, new signup, GitHub issue opened).
- **Job**: some work you want to do *reliably* (send email, update database, generate invoice).
- **Queue**: a named “line” of jobs (example: `stripe-payments`, `github-events`, `maintenance`, `csv-import`).
- **Worker**: a small program you run that **pulls jobs** from a queue and does the work.

Spooled is the “middle” that makes your automation reliable:

1. Something happens (Stripe payment, GitHub issue, nightly schedule, CSV import).
2. You create a **job** in Spooled.
3. Your worker processes it. If the worker is down → Spooled keeps the job and retries.

---

## One-time setup (do this once)

### 1) Create an account + API key

1. Go to `https://dashboard.spooled.cloud`
2. Create an account / log in
3. Create an API key

> Your real key usually starts with `sk_live_...` or `sk_test_...`.
> In docs we may show `sp_live_...` / `sp_test_...` to avoid GitHub false positives — **both formats are accepted** by the SDKs.

### 2) Get your “incoming webhook” URL + token (required)

Spooled gives each organization a unique incoming webhook URL:

- `POST /api/v1/webhooks/{org_id}/custom`
- required header: `X-Webhook-Token: ...`

Get your token + URL:

```bash
curl -sS https://api.spooled.cloud/api/v1/organizations/webhook-token \
  -H "Authorization: Bearer sp_test_YOUR_API_KEY"
```

You’ll get something like:

```json
{
  "webhook_token": "whk_abc123...",
  "webhook_url": "https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom"
}
```

Save:
- `SPOOLED_WEBHOOK_URL`
- `SPOOLED_WEBHOOK_TOKEN`

### 3) Send a test job (sanity check)

```bash
export SPOOLED_WEBHOOK_URL="https://api.spooled.cloud/api/v1/webhooks/org_abc123/custom"
export SPOOLED_WEBHOOK_TOKEN="whk_abc123..."

curl -sS -X POST "$SPOOLED_WEBHOOK_URL" \
  -H "Content-Type: application/json" \
  -H "X-Webhook-Token: $SPOOLED_WEBHOOK_TOKEN" \
  -d '{
    "queue_name": "test",
    "event_type": "hello.world",
    "idempotency_key": "hello-1",
    "payload": { "message": "Hello from curl!" }
  }'
```

Open the dashboard and look for a new job in the **`test`** queue.

### 4) Start a “super simple” worker (prints jobs to your terminal)

This worker listens to **one queue**, prints each job, and marks it as done.
No Slack, no email provider, no extra services.

#### A) Create a folder (Node.js)

```bash
mkdir -p spooled-worker && cd spooled-worker
npm init -y
npm install @spooled/sdk
```

#### B) Create `worker-print.mjs`

```js
import { SpooledWorker } from "@spooled/sdk";

const apiKey = process.env.SPOOLED_API_KEY;
const queueName = process.env.SPOOLED_QUEUE || "test";

if (!apiKey) throw new Error("Missing SPOOLED_API_KEY");

const worker = new SpooledWorker({
  apiKey,
  queueName,
  handler: async (job) => {
    console.log("---- New job ----");
    console.log("Queue:", queueName);
    console.log("Job ID:", job.id);
    console.log("Payload:", job.payload);
    console.log("-----------------");
    return { ok: true };
  },
});

await worker.start();
console.log(`✅ Worker started for queue: ${queueName}`);
```

#### C) Run it

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="test"
node worker-print.mjs
```

#### D) Test it

Re-run the “Send a test job” curl from step 3. You should see the job printed in your terminal.

---

## Example 1: Stripe payments → Spooled → your worker

### What this does

When somebody pays you in Stripe, you create a Spooled job.
Your worker can then do anything you want (print it, update your database, send an email, etc.).

### Stripe → your tiny webhook endpoint → Spooled

Stripe sends its own webhook JSON. Spooled expects a job with `queue_name` + `payload`.
So we add a tiny “translator” endpoint that:

1) receives the Stripe webhook  
2) verifies the Stripe signature  
3) creates a Spooled job

#### Step-by-step

1) Create a folder:

```bash
mkdir -p spooled-stripe-relay && cd spooled-stripe-relay
npm init -y
npm install express stripe
```

2) Create `server.mjs`:

```js
import express from "express";
import Stripe from "stripe";

const app = express();

const spooledApiKey = process.env.SPOOLED_API_KEY;
const spooledBaseUrl = process.env.SPOOLED_BASE_URL || "https://api.spooled.cloud";
const queueName = process.env.SPOOLED_QUEUE || "stripe-payments";

const stripeSecretKey = process.env.STRIPE_SECRET_KEY;
const stripeWebhookSecret = process.env.STRIPE_WEBHOOK_SECRET;

if (!spooledApiKey) throw new Error("Missing SPOOLED_API_KEY");
if (!stripeSecretKey) throw new Error("Missing STRIPE_SECRET_KEY");
if (!stripeWebhookSecret) throw new Error("Missing STRIPE_WEBHOOK_SECRET");

const stripe = new Stripe(stripeSecretKey);

// IMPORTANT: Stripe signature verification needs the RAW request body
app.post("/webhooks/stripe", express.raw({ type: "application/json" }), async (req, res) => {
  try {
    const sig = req.headers["stripe-signature"];
    if (!sig) return res.status(400).send("Missing Stripe-Signature header");

    const event = stripe.webhooks.constructEvent(req.body, sig, stripeWebhookSecret);

    const enqueue = await fetch(`${spooledBaseUrl}/api/v1/jobs`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${spooledApiKey}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        queue_name: queueName,
        idempotency_key: `stripe-${event.id}`,
        payload: event,
      }),
    });

    if (!enqueue.ok) {
      const body = await enqueue.text().catch(() => "");
      console.error("Spooled enqueue failed:", enqueue.status, body);
      return res.status(500).send("Spooled enqueue failed");
    }

    return res.status(200).send("ok");
  } catch (err) {
    console.error(err);
    return res.status(400).send(`Webhook error: ${err.message}`);
  }
});

app.listen(3000, () => console.log("✅ Stripe relay listening on http://localhost:3000/webhooks/stripe"));
```

3) Run it:

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="stripe-payments"
export STRIPE_SECRET_KEY="sk_test_YOUR_STRIPE_SECRET_KEY"
export STRIPE_WEBHOOK_SECRET="whsec_YOUR_STRIPE_WEBHOOK_SECRET"
node server.mjs
```

4) Test locally (easiest): forward webhooks to your laptop with Stripe CLI

If you don’t have Stripe CLI installed:

- macOS: `brew install stripe/stripe-cli/stripe`
- other OS: follow the Stripe CLI install guide

```bash
# In another terminal:
stripe listen --forward-to localhost:3000/webhooks/stripe
stripe trigger payment_intent.succeeded
```

5) Start your Spooled worker to see jobs:

```bash
cd ../spooled-worker
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="stripe-payments"
node worker-print.mjs
```

#### Production notes (simple)

For real production usage:

1) Deploy this small server publicly (any VPS/container that supports raw bodies)
2) In Stripe Dashboard → Developers → Webhooks:
   - Add endpoint: `https://YOUR_DOMAIN/webhooks/stripe`
   - Copy the signing secret (`whsec_...`) into `STRIPE_WEBHOOK_SECRET`

---

## Example 2: GitHub issue opened → Spooled → your worker (no server)

### What this does

When someone opens an issue in your GitHub repo, GitHub Actions sends a job to Spooled.
Your worker prints it.

### Step-by-step

1) In your GitHub repo: **Settings → Secrets and variables → Actions**

Add two repository secrets:
- `SPOOLED_WEBHOOK_URL`
- `SPOOLED_WEBHOOK_TOKEN`

2) Create `.github/workflows/spooled-issues.yml`:

```yaml
name: Spooled - enqueue GitHub issues

on:
  issues:
    types: [opened, reopened]

jobs:
  enqueue:
    runs-on: ubuntu-latest
    steps:
      - name: Send issue to Spooled
        env:
          SPOOLED_WEBHOOK_URL: ${{ secrets.SPOOLED_WEBHOOK_URL }}
          SPOOLED_WEBHOOK_TOKEN: ${{ secrets.SPOOLED_WEBHOOK_TOKEN }}
        run: |
          python - << 'PY' > payload.json
          import json, os
          event_path = os.environ["GITHUB_EVENT_PATH"]
          event = json.load(open(event_path))

          body = {
            "queue_name": "github-events",
            "event_type": "github.issue.opened",
            "idempotency_key": f"github-issue-{event['issue']['id']}",
            "payload": {
              "repo": os.environ.get("GITHUB_REPOSITORY"),
              "number": event["issue"]["number"],
              "title": event["issue"]["title"],
              "url": event["issue"]["html_url"],
              "author": event["issue"]["user"]["login"],
            },
          }
          print(json.dumps(body))
          PY

          curl -sS -X POST "$SPOOLED_WEBHOOK_URL" \
            -H "Content-Type: application/json" \
            -H "X-Webhook-Token: $SPOOLED_WEBHOOK_TOKEN" \
            --data-binary "@payload.json"
```

3) Start your worker:

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="github-events"
node worker-print.mjs
```

4) Test:
- Open a new GitHub issue
- The GitHub Action runs
- You see a job in the Spooled dashboard
- Your terminal prints the job

---

## Example 3: Run something on a schedule (cron) → Spooled → your worker

### What this does

Spooled can create jobs for you on a timer.
Example: “Every day at 9:00 AM, run cleanup”.

### Step-by-step

Spooled uses **6-field cron**: `second minute hour day month weekday`

Example: `0 0 9 * * *` = 09:00:00 every day.

1) Create a schedule:

```bash
curl -sS -X POST https://api.spooled.cloud/api/v1/schedules \
  -H "Authorization: Bearer sp_test_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "daily-cleanup",
    "cron_expression": "0 0 9 * * *",
    "timezone": "UTC",
    "queue_name": "maintenance",
    "payload_template": { "task": "cleanup_temp_files" }
  }'
```

2) Start your worker:

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="maintenance"
node worker-print.mjs
```

3) (Optional) Trigger it immediately:

```bash
curl -sS -X POST "https://api.spooled.cloud/api/v1/schedules/SCHEDULE_ID/trigger" \
  -H "Authorization: Bearer sp_test_YOUR_API_KEY"
```

---

## Example 4: Process a CSV file in the background → Spooled → your worker

### What this does

“I have a CSV with many rows, process each row reliably.”

### Step-by-step

1) Create `users.csv`:

```csv
email,name
alice@example.com,Alice
bob@example.com,Bob
```

2) Create `enqueue-csv.mjs` (inside `spooled-worker/`):

```js
import fs from "node:fs/promises";

const apiKey = process.env.SPOOLED_API_KEY;
const baseUrl = process.env.SPOOLED_BASE_URL || "https://api.spooled.cloud";
const queueName = process.env.SPOOLED_QUEUE || "csv-import";
const file = process.argv[2];

if (!apiKey) throw new Error("Missing SPOOLED_API_KEY");
if (!file) throw new Error("Usage: node enqueue-csv.mjs <file.csv>");

const text = await fs.readFile(file, "utf8");
const lines = text.trim().split(/\r?\n/);
const headers = lines[0].split(",").map((s) => s.trim());
const rows = lines.slice(1);

for (const line of rows) {
  if (!line.trim()) continue;
  const values = line.split(",").map((s) => s.trim());

  const payload = {};
  for (let i = 0; i < headers.length; i++) payload[headers[i]] = values[i] || "";

  const idempotencyKey = payload.email ? `csv-${payload.email}` : undefined;

  const res = await fetch(`${baseUrl}/api/v1/jobs`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${apiKey}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ queue_name: queueName, idempotency_key: idempotencyKey, payload }),
  });

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`Failed to enqueue job: ${res.status} ${body}`);
  }
}

console.log(`✅ Enqueued ${rows.length} jobs to queue: ${queueName}`);
```

3) Enqueue jobs:

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="csv-import"
node enqueue-csv.mjs users.csv
```

4) Process them:

```bash
export SPOOLED_API_KEY="sp_test_YOUR_API_KEY"
export SPOOLED_QUEUE="csv-import"
node worker-print.mjs
```

---

## Example 5: Your website signup → Spooled → background work

### What this does

When a user signs up, your app should respond fast.
So you enqueue a job and do slow stuff in the background.

### REST API (works in any language)

```bash
curl -sS -X POST https://api.spooled.cloud/api/v1/jobs \
  -H "Authorization: Bearer sp_test_YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "queue_name": "signups",
    "payload": {
      "user_id": "user_123",
      "email": "alice@example.com"
    },
    "idempotency_key": "signup-user_123"
  }'
```

Start a worker for queue `signups` (for example `worker-print.mjs`) and you’re done.

---

## Tips (important, but simple)

- Always set `idempotency_key` when the event has an ID (Stripe event ID, GitHub issue ID, user ID, email, etc.)
- Keep your `X-Webhook-Token` secret. Don’t put it in public repos.
- Start simple: first print jobs, then add real actions.
- You don’t need to pre-create queues. Pick a queue name and send jobs to it.


