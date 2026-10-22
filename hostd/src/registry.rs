//! The capabilities the planner's JSON actions can call: one list that the executor dispatches on
//! (exhaustively, so every listed action has an implementation) and that generates the catalog the
//! planner is shown, so the set advertised is the set executed.

use serde_json::{Value, json};

use crate::budget::Kind;

/// One capability a plan can call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    FsListDir,
    FsReadFile,
    ProcSpawn,
    BrowserOpenSession,
    BrowserGoto,
    BrowserDescribePage,
    BrowserFind,
    BrowserClick,
    BrowserTypeText,
    BrowserInnerText,
    BrowserScreenshot,
}

impl Action {
    pub const ALL: [Action; 11] = [
        Action::FsListDir,
        Action::FsReadFile,
        Action::ProcSpawn,
        Action::BrowserOpenSession,
        Action::BrowserGoto,
        Action::BrowserDescribePage,
        Action::BrowserFind,
        Action::BrowserClick,
        Action::BrowserTypeText,
        Action::BrowserInnerText,
        Action::BrowserScreenshot,
    ];

    /// The name a plan uses.
    pub fn name(self) -> &'static str {
        match self {
            Action::FsListDir => "fs.list_dir",
            Action::FsReadFile => "fs.read_file",
            Action::ProcSpawn => "proc.spawn",
            Action::BrowserOpenSession => "browser.open_session",
            Action::BrowserGoto => "browser.session.goto",
            Action::BrowserDescribePage => "browser.session.describe_page",
            Action::BrowserFind => "browser.session.find",
            Action::BrowserClick => "browser.element.click",
            Action::BrowserTypeText => "browser.element.type_text",
            Action::BrowserInnerText => "browser.element.inner_text",
            Action::BrowserScreenshot => "browser.session.screenshot",
        }
    }

    pub fn parse(name: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The budget each call is paid from.
    pub fn budget(self) -> Kind {
        match self {
            Action::FsListDir | Action::FsReadFile => Kind::FilesystemOps,
            Action::ProcSpawn => Kind::Processes,
            _ => Kind::BrowserActions,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Action::FsListDir => "List a directory in the workspace (default: its root).",
            Action::FsReadFile => "Read a workspace file (at most max_bytes, 4096 by default).",
            Action::ProcSpawn => {
                "Run an allowlisted program to completion; env names must be in the host's \
                 allow_env, and values may be {{secret:NAME}} references."
            }
            Action::BrowserOpenSession => "Open a browser session under a new alias.",
            Action::BrowserGoto => "Visit a URL on the host's allowed hosts.",
            Action::BrowserDescribePage => {
                "Report the page's URL and title, and its HTML if asked."
            }
            Action::BrowserFind => "Find the first element a selector matches, under a new alias.",
            Action::BrowserClick => "Click an element.",
            Action::BrowserTypeText => {
                "Type text into an element (it may hold {{secret:NAME}} references), optionally \
                 pressing Enter."
            }
            Action::BrowserInnerText => "Read an element's text.",
            Action::BrowserScreenshot => "Take a PNG screenshot of the page.",
        }
    }

    /// The input's JSON schema.
    fn input(self) -> Value {
        let string = json!({ "type": "string" });
        let ms = json!({ "type": "integer", "minimum": 0 });
        let object = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
        let selector = object(
            json!({
                "kind": { "enum": ["css", "xpath", "text"] },
                "value": string,
            }),
            &["kind", "value"],
        );
        match self {
            Action::FsListDir => object(json!({ "path": string }), &[]),
            Action::FsReadFile => object(
                json!({ "path": string, "max_bytes": { "type": "integer", "minimum": 0 } }),
                &["path"],
            ),
            Action::ProcSpawn => object(
                json!({
                    "command": string,
                    "args": { "type": "array", "items": string },
                    "cwd": string,
                    "env": {
                        "type": "array",
                        "items": object(json!({ "key": string, "value": string }), &["key", "value"]),
                    },
                    "timeout_ms": ms,
                }),
                &["command"],
            ),
            Action::BrowserOpenSession => object(
                json!({
                    "alias": string,
                    "headless": { "type": "boolean" },
                    "allow_downloads": { "type": "boolean" },
                }),
                &["alias"],
            ),
            Action::BrowserGoto => object(
                json!({ "session": string, "url": string, "timeout_ms": ms }),
                &["session", "url"],
            ),
            Action::BrowserDescribePage => object(
                json!({ "session": string, "include_html": { "type": "boolean" } }),
                &["session"],
            ),
            Action::BrowserFind => object(
                json!({ "session": string, "alias": string, "selector": selector, "timeout_ms": ms }),
                &["session", "alias", "selector"],
            ),
            Action::BrowserClick | Action::BrowserInnerText => {
                object(json!({ "element": string }), &["element"])
            }
            Action::BrowserTypeText => object(
                json!({ "element": string, "text": string, "submit": { "type": "boolean" } }),
                &["element", "text"],
            ),
            Action::BrowserScreenshot => object(json!({ "session": string }), &["session"]),
        }
    }
}

/// The catalog the planner is shown: each capability's name, description and input schema.
pub fn catalog() -> Value {
    Value::Array(
        Action::ALL
            .iter()
            .map(
                |a| json!({ "name": a.name(), "description": a.description(), "input": a.input() }),
            )
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog lists exactly the actions the executor dispatches on (an exhaustive match over
    /// [`Action`]), each once, by a name that parses back.
    #[test]
    fn the_catalog_lists_exactly_the_executors_actions() {
        let names: Vec<String> = catalog()
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect();
        let expected: Vec<String> = Action::ALL.iter().map(|a| a.name().to_string()).collect();
        assert_eq!(names, expected);
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "no name twice");
        for a in Action::ALL {
            assert_eq!(Action::parse(a.name()), Some(a));
        }
        assert_eq!(Action::parse("fs.delete_everything"), None);
    }

    #[test]
    fn every_entry_has_a_description_and_an_object_schema() {
        for entry in catalog().as_array().unwrap() {
            assert!(
                !entry["description"].as_str().unwrap().is_empty(),
                "{entry}"
            );
            assert_eq!(entry["input"]["type"], "object", "{entry}");
        }
    }
}
