//! Drives the installed browsers through the MCP tools. Skips a browser
//! that is not installed.

use cmdterm::devtools::mcp::{Caller, handle};
use cmdterm::devtools::{Choice, Config, hub};
use serde_json::{Value, json};

const PAGE: &str = "data:text/html,<title>Form</title><h1>Hello</h1>\
<label for=n>Name</label><input id=n>\
<button onclick=\"document.querySelector('h1').textContent='Hi '+n.value;console.log('clicked',n.value)\">Greet</button>";

fn call(name: &str, arguments: Value) -> (bool, String, Value) {
    let message = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let caller = Caller {
        host: true,
        ..Caller::default()
    };
    let answer = handle(hub(), &message, &caller).expect("a call gets an answer");
    let result = answer["result"].clone();
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (result["isError"] == true, text, result)
}

/// The ref of the line of `snapshot` that contains `needle`.
fn reference(snapshot: &str, needle: &str) -> String {
    let line = snapshot
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no {needle} in\n{snapshot}"));
    let start = line.find("[ref=").expect("a ref") + 5;
    line[start..].split(']').next().unwrap().to_owned()
}

fn drive(choice: Choice) {
    let profiles = tempfile::tempdir().unwrap();
    hub().configure(Config {
        browser: choice,
        executable: None,
        headless: true,
        profile_dir: profiles.path().to_path_buf(),
    });
    let (failed, text, _) = call("browser_session_new", json!({ "url": PAGE }));
    assert!(!failed, "{text}");
    let session = text
        .split("Started session ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap()
        .to_owned();
    assert!(text.contains("heading \"Hello\" [level=1]"), "{text}");
    let input = reference(&text, "textbox \"Name\"");
    let (failed, text, _) = call(
        "browser_type",
        json!({ "session": session, "ref": input, "text": "Ada" }),
    );
    assert!(!failed, "{text}");
    assert!(text.contains(": \"Ada\""), "{text}");
    let button = reference(&text, "button \"Greet\"");
    let (failed, text, _) = call(
        "browser_click",
        json!({ "session": session, "ref": button }),
    );
    assert!(!failed, "{text}");
    assert!(text.contains("heading \"Hi Ada\""), "{text}");
    let (_, text, _) = call(
        "browser_evaluate",
        json!({ "session": session, "function": "() => document.title" }),
    );
    assert!(text.contains("\"Form\""), "{text}");
    let (_, text, _) = call("browser_console_messages", json!({ "session": session }));
    assert!(text.contains("clicked Ada"), "{text}");
    let (failed, _, result) = call("browser_take_screenshot", json!({ "session": session }));
    assert!(!failed);
    assert_eq!(result["content"][1]["mimeType"], "image/png");
    let (_, text, _) = call(
        "browser_press_key",
        json!({ "session": session, "key": "Control+a" }),
    );
    assert!(text.contains("Pressed"), "{text}");
    let (failed, text, _) = call(
        "browser_tabs",
        json!({ "session": session, "action": "new" }),
    );
    assert!(!failed, "{text}");
    assert!(text.contains("- 1: about:blank"), "{text}");
    let (failed, text, _) = call("browser_session_close", json!({ "session": session }));
    assert!(!failed, "{text}");
    let (failed, text, _) = call("browser_snapshot", json!({ "session": session }));
    assert!(failed && text.contains("no session"), "{text}");
    hub().shutdown();
}

#[test]
fn chrome_and_firefox_answer_the_tools() {
    if which("chromium")
        .or_else(|| which("google-chrome"))
        .is_some()
    {
        drive(Choice::Chrome);
    }
    if which("firefox").is_some() {
        drive(Choice::Firefox);
    }
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
