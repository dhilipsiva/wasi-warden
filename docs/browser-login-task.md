## Browser Login Sample Task

This walkthrough shows how to use the existing WIT contracts to automate a “sign in and verify” flow. The goal: open a site, trigger its login UI, submit credentials, and confirm that the session switched to an authenticated state — all while staying inside `wasi-warden`’s capability boundaries.

---

### 1. Policy + config expectations

- Browser capability must be enabled in `hostd.toml`. The minimal block looks like:

  ```toml
  [browser]
  webdriver_url = "http://127.0.0.1:9515" # chromedriver/geckodriver/etc.
  default_profile = "default"
  ```

- Workspace should contain the credential material the planner is allowed to read, e.g. `secrets/demo-login.json`.
- Optional: allow `fs` read-only access to `secrets/`, block write/delete to prevent credential exfiltration.
- Input automation stays disabled; all typing happens through browser element handles.

Sample invocation:

```bash
cargo run -p hostd -- step \
  --task "Log into https://demo.example/login with creds in secrets/demo-login.json and confirm the dashboard greets the user." \
  --observation '{}'
```

---

### 2. Capabilities touched

| Capability | Purpose |
|------------|---------|
| `fs.read_file` | Load username/password JSON from workspace secrets. |
| `browser.open-session` | Spawn a headless Chromium/WebDriver session. |
| `browser.session.goto` | Navigate to the login page. |
| `browser.session.find` | Locate buttons/inputs using CSS or XPath. |
| `browser.element.click` | Open modal or submit buttons. |
| `browser.element.type-text` | Fill username/password; set `submit = false` except for the final Enter. |
| `browser.session.describe-page` | Snapshot the DOM to verify logged-in state. |
| `browser.session.screenshot` (optional) | Capture proof of success for auditors. |

Future policy hooks (`policy.request_capability`, `policy.log_event`) can wrap each sensitive step for human-on-the-loop approval.

---

### 3. Planner action plan (example)

The planner returns structured JSON actions. Below is a realistic sequence assuming the site requires clicking a “Sign in” button before the form appears. Replace selectors to match the target site.

```json
{
  "thought": "Load credentials, open the login form, submit, then confirm dashboard greeting.",
  "actions": [
    {
      "capability": "fs.read_file",
      "input": {
        "path": "secrets/demo-login.json",
        "max_bytes": 2048
      }
    },
    {
      "capability": "browser.open_session",
      "input": {
        "profile": "default",
        "headless": true,
        "allow_downloads": false
      }
    },
    {
      "capability": "browser.session.goto",
      "input": {
        "session": "session-1",
        "url": "https://demo.example/login",
        "timeout_ms": 15000
      }
    },
    {
      "capability": "browser.session.find",
      "input": {
        "session": "session-1",
        "selector": { "kind": "css", "value": "button[data-test='signin']" },
        "timeout_ms": 5000,
        "alias": "signin-btn"
      }
    },
    {
      "capability": "browser.element.click",
      "input": { "session": "session-1", "element": "signin-btn" }
    },
    {
      "capability": "browser.session.find",
      "input": {
        "session": "session-1",
        "selector": { "kind": "css", "value": "input#username" },
        "timeout_ms": 5000,
        "alias": "username-field"
      }
    },
    {
      "capability": "browser.element.type_text",
      "input": {
        "session": "session-1",
        "element": "username-field",
        "text": "{{creds.username}}",
        "submit": false
      }
    },
    {
      "capability": "browser.session.find",
      "input": {
        "session": "session-1",
        "selector": { "kind": "css", "value": "input#password" },
        "timeout_ms": 5000,
        "alias": "password-field"
      }
    },
    {
      "capability": "browser.element.type_text",
      "input": {
        "session": "session-1",
        "element": "password-field",
        "text": "{{creds.password}}",
        "submit": true
      }
    },
    {
      "capability": "browser.session.describe_page",
      "input": {
        "session": "session-1",
        "include_html": true
      }
    },
    {
      "capability": "browser.session.find",
      "input": {
        "session": "session-1",
        "selector": { "kind": "css", "value": "[data-test='user-greeting']" },
        "timeout_ms": 8000,
        "alias": "greeting"
      }
    },
    {
      "capability": "browser.element.inner_text",
      "input": { "session": "session-1", "element": "greeting" }
    },
    {
      "capability": "browser.session.screenshot",
      "input": {
        "session": "session-1",
        "kind": "png",
        "alias": "post-login"
      }
    }
  ]
}
```

Notes:

- `alias` illustrates how the planner can refer to handles in its own reasoning; the host implementation holds the actual resource handles.
- Every browser action that produces a new handle (`browser.open_session`, `browser.session.find`, `browser.session.query_all`, etc.) **must** provide a unique `alias`. Reuse that alias via fields like `session` or `element` in subsequent actions.
- Credential interpolation (`{{creds.username}}`) is done inside the planner after it parses the `fs.read_file` result; the host only sees the literal strings sent via `type_text`.
- Verification condition is satisfied when `inner_text` contains a phrase like `"Welcome, Demo User"` and the planner returns a `complete` response with that evidence plus the screenshot metadata in `outcome`.

---

### 4. Observation contract

The host should feed each capability result back through the observation JSON, e.g.:

```json
{
  "actions": [
    { "capability": "fs.read_file", "success": true },
    { "capability": "browser.element.inner_text", "success": true, "output": { "text": "Welcome, Demo User" } }
  ]
}
```

The planner inspects this structure to decide whether to continue or finish. For login flows, the success predicate might be:

1. `browser.element.inner_text` contains the expected greeting, **and**
2. `browser.session.describe_page` shows `url` rooted under `/dashboard`.

If either check fails, the planner can emit new actions (e.g., re-open the menu, retry credentials, capture more diagnostics) while staying within the same capability set.
