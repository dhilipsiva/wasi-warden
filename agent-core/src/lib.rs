use serde::Deserialize;
use serde_json::Value;

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "control",
        generate_all,
        default_bindings_module: "bindings",
    });
}

use bindings::exports::osagent::agent::planner::{
    self, AgentError, CompletePlan, ContinuePlan, PlannedAction, StepResponse,
};
use bindings::osagent::common::types::{CapabilityError, CapabilityErrorCode};
use bindings::osagent::llm::llm::{self, Message, Role};

const SYSTEM_PROMPT: &str = r#"
You are an expert automation planner operating inside a secure agent runtime.
Respond ONLY with JSON matching this schema:
{
  "status": "continue" | "complete",
  "thought": "human-readable reasoning",
  "actions": [
     { "capability": "fs.list_dir" | "proc.spawn" | "fs.read_file",
       "input": { ... json arguments ... }
     }
  ],
  "result": { ... final result json when status == "complete" },
  "reason": "short explanation when status == \"complete\""
}
When status is \"continue\" you MUST include at least one action describing the next capability call.
Capabilities available:
- fs.list_dir { "path": "<relative path>" }
- fs.read_file { "path": "<relative path>", "max_bytes": 4096 }
- proc.spawn { "command": "<program>", "args": ["..."] }
Always keep paths relative to the provided workspace.
"#;

struct Agent;

impl planner::Guest for Agent {
    fn step(task: String, observation: planner::Observation) -> Result<StepResponse, AgentError> {
        plan_with_llm(task, observation).map_err(agent_error)
    }
}

bindings::export!(Agent);

fn plan_with_llm(
    task: String,
    observation: planner::Observation,
) -> Result<StepResponse, AgentErr> {
    let messages = build_messages(&task, &observation);
    let options = llm::Options {
        max_tokens: Some(600),
        temperature: Some(0.2),
        top_p: Some(0.9),
        stop: Vec::new(),
        presence_penalty: None,
        frequency_penalty: None,
    };
    let completion = llm::complete(&messages, &options).map_err(cap_err("llm.complete"))?;
    plan_from_content(&completion.content)
}

/// The JSON inside a Markdown code fence (```` ```json ```` or a bare ```` ``` ````), or the reply
/// itself when it is not fenced. Models often fence their JSON despite the prompt.
fn strip_code_fence(content: &str) -> &str {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return trimmed;
    };
    // The opening fence's line may name a language (`json`); the JSON starts on the next line.
    match body.split_once('\n') {
        Some((info, json)) if !info.trim_start().starts_with('{') => json.trim(),
        _ => body.trim(),
    }
}

/// The step response the model's reply `content` describes.
fn plan_from_content(content: &str) -> Result<StepResponse, AgentErr> {
    let envelope: PlanEnvelope =
        serde_json::from_str(strip_code_fence(content)).map_err(|err| {
            AgentErr::fatal(format!(
                "failed to parse LLM response: {err}; content: {content}"
            ))
        })?;

    match envelope.status {
        PlanStatus::Continue => {
            let actions = envelope
                .actions
                .ok_or_else(|| AgentErr::fatal("LLM continuation missing actions"))?;
            if actions.is_empty() {
                return Err(AgentErr::fatal("LLM returned no actions"));
            }
            let planned = actions
                .into_iter()
                .map(to_planned_action)
                .collect::<Result<_, _>>()?;
            let thought = envelope
                .thought
                .unwrap_or_else(|| "No reasoning provided.".to_string());
            Ok(StepResponse::Continue(ContinuePlan {
                thought,
                actions: planned,
            }))
        }
        PlanStatus::Complete => {
            let reason = envelope
                .reason
                .or(envelope.thought)
                .unwrap_or_else(|| "task complete".to_string());
            let outcome = envelope.result.unwrap_or(Value::Null).to_string();
            Ok(StepResponse::Complete(CompletePlan { reason, outcome }))
        }
    }
}

fn build_messages(task: &str, observation: &planner::Observation) -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: SYSTEM_PROMPT.to_string(),
            name: None,
        },
        Message {
            role: Role::User,
            content: format!(
                "Task: {task}\nCurrent step #: {}\nLast observation summary: {}\nObservation data: {}",
                observation.step, observation.summary, observation.data
            ),
            name: None,
        },
    ]
}

fn to_planned_action(action: LlmAction) -> Result<PlannedAction, AgentErr> {
    Ok(PlannedAction {
        capability: action.capability,
        input: action.input.to_string(),
        audit_tag: None,
    })
}

fn cap_err(op: &'static str) -> impl Fn(CapabilityError) -> AgentErr {
    move |err| {
        let retryable = matches!(err.code, CapabilityErrorCode::Unavailable);
        AgentErr::new(
            retryable,
            format!("{op} failed: {} ({:?})", err.message, err.code),
        )
    }
}

fn agent_error(err: AgentErr) -> AgentError {
    AgentError {
        retryable: err.retryable,
        message: err.message,
    }
}

struct AgentErr {
    retryable: bool,
    message: String,
}

impl AgentErr {
    fn new(retryable: bool, message: impl Into<String>) -> Self {
        Self {
            retryable,
            message: message.into(),
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self::new(false, message)
    }
}

#[derive(Deserialize)]
struct PlanEnvelope {
    status: PlanStatus,
    thought: Option<String>,
    actions: Option<Vec<LlmAction>>,
    result: Option<Value>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum PlanStatus {
    Continue,
    Complete,
}

#[derive(Deserialize)]
struct LlmAction {
    capability: String,
    input: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(content: &str) -> Result<StepResponse, String> {
        plan_from_content(content).map_err(|e| e.message)
    }

    const CONTINUE: &str = r#"{"status":"continue","thought":"look","actions":[{"capability":"fs.list_dir","input":{"path":"."}}]}"#;

    #[test]
    fn a_continue_plan_carries_its_actions() {
        match plan(CONTINUE).unwrap() {
            StepResponse::Continue(p) => {
                assert_eq!(p.thought, "look");
                assert_eq!(p.actions.len(), 1);
                assert_eq!(p.actions[0].capability, "fs.list_dir");
                assert_eq!(p.actions[0].input, r#"{"path":"."}"#);
            }
            StepResponse::Complete(_) => panic!("expected a continue plan"),
        }
    }

    #[test]
    fn a_continue_plan_needs_at_least_one_action() {
        assert!(plan(r#"{"status":"continue","thought":"t"}"#).is_err());
        assert!(plan(r#"{"status":"continue","thought":"t","actions":[]}"#).is_err());
    }

    #[test]
    fn a_complete_plan_takes_its_reason_then_its_thought() {
        let done = |content| match plan(content).unwrap() {
            StepResponse::Complete(c) => (c.reason, c.outcome),
            StepResponse::Continue(_) => panic!("expected completion"),
        };
        assert_eq!(
            done(r#"{"status":"complete","reason":"r","thought":"t","result":{"x":1}}"#),
            ("r".to_string(), r#"{"x":1}"#.to_string())
        );
        assert_eq!(
            done(r#"{"status":"complete","thought":"t"}"#),
            ("t".to_string(), "null".to_string())
        );
    }

    #[test]
    fn prose_is_not_a_plan() {
        let err = plan("I will list the files now.").unwrap_err();
        assert!(err.contains("failed to parse LLM response"), "{err}");
        assert!(plan(r#"{"status":"thinking"}"#).is_err());
    }

    /// Models often wrap JSON in a Markdown code fence despite the prompt.
    #[test]
    fn a_plan_in_a_json_code_fence_is_accepted() {
        let fenced = format!("```json\n{CONTINUE}\n```");
        assert!(
            matches!(plan(&fenced), Ok(StepResponse::Continue(_))),
            "{fenced}"
        );
    }

    #[test]
    fn a_fence_is_stripped_only_when_it_closes() {
        assert_eq!(strip_code_fence("```json\n{}\n```"), "{}");
        assert_eq!(strip_code_fence("```{\"a\":1}```"), "{\"a\":1}");
        assert_eq!(
            strip_code_fence("```json\n{}"),
            "```json\n{}",
            "an unclosed fence is left alone"
        );
        assert_eq!(strip_code_fence("  {}  "), "{}");
    }

    #[test]
    fn a_plan_in_a_bare_code_fence_is_accepted() {
        let fenced = format!("  ```\n{CONTINUE}\n```\n");
        assert!(
            matches!(plan(&fenced), Ok(StepResponse::Continue(_))),
            "{fenced}"
        );
    }
}
