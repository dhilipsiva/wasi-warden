## Browser Login Sample Task

This walkthrough shows a "sign in and verify" flow: open a site, trigger its login UI, submit credentials,
and confirm that the session switched to an authenticated state, all inside `wasi-warden`'s capability
boundaries. The credentials never reach the model: the plan names them, and the host fills them in.

`hostd/tests/browser_e2e.rs` (`the_login_walkthrough_runs`) runs the plan below, exactly as written here,
against a fake WebDriver, so this page stays true.

---

### 1. Configuration

- The browser capability must be enabled, and the site's host allowed:

  ```toml
  [browser]
  webdriver_url = "http://127.0.0.1:9515" # chromedriver, geckodriver, ...
  allowed_hosts = ["demo.example"]        # the only hosts the session may visit
  ```

- The credentials are host-side secrets, never files in the workspace:

  ```toml
  [secrets]
  demo_username = "demo-user"
  demo_password = { env = "DEMO_PASSWORD" } # or read from the host's environment
  ```

  A plan names a secret as `{{secret:NAME}}`. The host replaces the reference with the value only in text
  typed into a page element (`browser.element.type_text`) and in a spawned process's environment values, and
  redacts every secret value back to its reference from everything bound for the model, the observations,
  the audit log and the log lines.
- Input automation stays disabled; all typing happens through browser element handles.

Sample invocation:

```bash
cargo run -p hostd -- step \
  --task "Log into https://demo.example/login with the demo_username and demo_password secrets and confirm the dashboard greets the user." \
  --observation '{}'
```

---

### 2. Capabilities touched

| Capability | Purpose |
|------------|---------|
| `browser.open_session` | Open a headless Chromium session through the WebDriver. |
| `browser.session.goto` | Navigate to the login page (an allowed host). |
| `browser.session.find` | Locate buttons and inputs by CSS, XPath or text. |
| `browser.element.click` | Open the sign-in form. |
| `browser.element.type_text` | Fill the username and password from secrets; `submit = true` presses Enter. |
| `browser.session.describe_page` | Snapshot the page to verify the signed-in state. |
| `browser.element.inner_text` | Read the greeting. |
| `browser.session.screenshot` | Capture proof of success. |

---

### 3. Planner action plan

The planner returns structured JSON actions. Below is a realistic sequence for a site that needs a "Sign in"
click before the form appears; replace the selectors to match the target site.

```json
{
  "status": "continue",
  "thought": "Open the login form, sign in with the host's secrets, then confirm the dashboard greeting.",
  "actions": [
    {
      "capability": "browser.open_session",
      "input": { "alias": "session-1", "headless": true, "allow_downloads": false }
    },
    {
      "capability": "browser.session.goto",
      "input": { "session": "session-1", "url": "https://demo.example/login", "timeout_ms": 0 }
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
      "input": { "element": "signin-btn" }
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
      "input": { "element": "username-field", "text": "{{secret:demo_username}}", "submit": false }
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
      "input": { "element": "password-field", "text": "{{secret:demo_password}}", "submit": true }
    },
    {
      "capability": "browser.session.describe_page",
      "input": { "session": "session-1", "include_html": true }
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
      "input": { "element": "greeting" }
    },
    {
      "capability": "browser.session.screenshot",
      "input": { "session": "session-1" }
    }
  ]
}
```

Notes:

- Every action that creates a handle (`browser.open_session`, `browser.session.find`) names it with a unique
  `alias`; later actions refer to it through `session` or `element`.
- The planner never sees the credentials: it writes `{{secret:demo_username}}`, and the host types the value.
  If a page or a process echoes a secret back, the host redacts it before the observation reaches the planner.
- The goal is met when `inner_text` contains the expected greeting (for example `"Welcome, Demo User"`); the
  planner then returns a `complete` response with that evidence in `result`.

---

### 4. Observation contract

The host feeds each action's result back through the observation JSON, for example:

```text
{
  "actions": [
    { "capability": "browser.element.type_text", "success": true, "output": { "element": "password-field" } },
    { "capability": "browser.element.inner_text", "success": true, "output": { "text": "Welcome, Demo User" } }
  ]
}
```

The planner inspects it to decide whether to continue or finish. For a login flow the success predicate
might be:

1. `browser.element.inner_text` contains the expected greeting, **and**
2. `browser.session.describe_page` shows a `url` under `/dashboard`.

If either check fails, the planner can emit new actions (re-open the menu, capture more diagnostics) within
the same capability set and the run's budgets.
