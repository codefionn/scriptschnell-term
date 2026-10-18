use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::ext::output::PrefixWriter;
use crate::ext::{
    BranchReport, BranchState, Config, Context, Event, ExtensionHost, ForgeCheck, HttpRequest,
    HttpResponse, JobResult, KeyRequest, NotifyLevel, Permission, Permissions, Request,
};

fn drain(host: &mut ExtensionHost) -> Vec<Request> {
    let mut out = Vec::new();
    host.drain_requests(&mut out);
    out
}

/// The messages of all queued `Notify` requests.
fn notes(host: &mut ExtensionHost) -> Vec<String> {
    drain(host)
        .into_iter()
        .filter_map(|request| match request {
            Request::Notify { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn enabled(host: &ExtensionHost, name: &str) -> bool {
    host.extensions()
        .find(|ext| ext.name == name)
        .is_some_and(|ext| ext.enabled)
}

#[test]
fn loads_init_lua_directories_and_single_files() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("alpha");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(
        nested.join("helper.lua"),
        "return { greet = 'hi from helper' }",
    )
    .unwrap();
    std::fs::write(
        nested.join("init.lua"),
        "local helper = require('helper') term.notify(helper.greet)",
    )
    .unwrap();
    std::fs::write(dir.path().join("beta.lua"), "term.notify('beta loaded')").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not lua").unwrap();
    std::fs::create_dir(dir.path().join("empty")).unwrap();

    let mut host = ExtensionHost::new();
    let errors = host.load_dir(dir.path());
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<_> = host.extensions().map(|ext| ext.name.clone()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert!(host.extensions().all(|ext| ext.enabled));
    assert_eq!(
        host.extensions().next().unwrap().path.as_deref(),
        Some(nested.join("init.lua").as_path())
    );
    assert_eq!(notes(&mut host), ["hi from helper", "beta loaded"]);
}

#[test]
fn turned_off_extensions_are_listed_but_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.lua"), "term.notify('alpha')").unwrap();
    std::fs::write(dir.path().join("beta.lua"), "term.notify('beta')").unwrap();

    let mut host = ExtensionHost::new();
    let errors = host.load_dir_except(
        dir.path(),
        &["alpha".to_owned()],
        &Default::default(),
        &Default::default(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let listed: Vec<_> = host
        .extensions()
        .map(|ext| (ext.name.as_str(), ext.enabled, ext.turned_off))
        .collect();
    assert_eq!(listed, [("alpha", false, true), ("beta", true, false)]);
    assert_eq!(notes(&mut host), ["beta"]);
    assert_eq!(
        host.extensions().next().unwrap().path.as_deref(),
        Some(dir.path().join("alpha.lua").as_path())
    );
}

#[test]
fn extensions_off_by_default_load_once_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let quiet = dir.path().join("quiet");
    std::fs::create_dir(&quiet).unwrap();
    std::fs::write(
        quiet.join("manifest.toml"),
        "description = \"off at first\"\nenabled = false",
    )
    .unwrap();
    std::fs::write(quiet.join("init.lua"), "term.notify('quiet')").unwrap();

    let mut host = ExtensionHost::new();
    assert!(
        host.load_dir_except(dir.path(), &[], &Default::default(), &Default::default())
            .is_empty()
    );
    let ext = host.extensions().next().unwrap();
    assert!(ext.turned_off && !ext.default_on);
    assert_eq!(ext.description.as_deref(), Some("off at first"));
    assert!(notes(&mut host).is_empty());

    let mut host = ExtensionHost::new();
    host.turn_on(&["quiet".to_owned()]);
    host.load_dir_except(dir.path(), &[], &Default::default(), &Default::default());
    assert!(!host.extensions().next().unwrap().turned_off);
    assert_eq!(notes(&mut host), ["quiet"]);

    // Off wins over on.
    let mut host = ExtensionHost::new();
    host.turn_on(&["quiet".to_owned()]);
    host.load_dir_except(
        dir.path(),
        &["quiet".to_owned()],
        &Default::default(),
        &Default::default(),
    );
    assert!(host.extensions().next().unwrap().turned_off);
}

#[test]
fn done_functions_get_the_result_once() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "steps",
        r#"
        term.command{ name = "steps.go", run = function()
            term.run{ cmd = { "dotnet", "--version" }, host = true, done = function(r)
                term.notify(tostring(r.ok) .. "|" .. tostring(r.code) .. "|" .. r.stdout .. "|" .. tostring(r.error))
                term.copy{ from = "/a", to = "~", done = function(c)
                    term.notify("copy " .. tostring(c.ok) .. " " .. c.error)
                end }
            end }
        end }
        "#,
    )
    .unwrap();
    host.run_command("steps.go").unwrap();
    let requests = drain(&mut host);
    let [
        Request::Run {
            on_host: true,
            done: Some(done),
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    let done = *done;
    host.complete(done, &JobResult::from_output(Some(0), b"9.0.100", b""))
        .unwrap();
    // A second completion of the same callback does nothing.
    host.complete(done, &JobResult::from_output(Some(1), b"", b""))
        .unwrap();
    let requests = drain(&mut host);
    let [
        Request::Notify { message, .. },
        Request::Copy {
            done: Some(copied), ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    assert_eq!(message, "true|0|9.0.100|nil");
    let copied = *copied;
    host.complete(copied, &JobResult::failed("no tar")).unwrap();
    assert_eq!(notes(&mut host), ["copy false no tar"]);
}

#[test]
fn host_runs_and_trust_need_their_permissions() {
    let mut host = ExtensionHost::new();
    let run_only = Permissions::from_names(&["run"]);
    host.load_source_with(
        "a",
        r#"term.command{ name = "a.host", run = function() term.run{ cmd = { "x" }, host = true } end }
           term.command{ name = "a.trust", run = function() term.trust{ name = "dev", path = "/c.pem" } end }
           term.command{ name = "a.local", run = function() term.run{ cmd = { "x" } } end }"#,
        run_only,
    )
    .unwrap();
    assert!(host.run_command("a.local").is_ok());
    let error = host.run_command("a.host").unwrap_err();
    assert!(error.message.contains("'process' permission"), "{error}");

    let mut host = ExtensionHost::new();
    host.load_source_with(
        "b",
        r#"term.command{ name = "b.trust", run = function() term.trust{ name = "dev", path = "/c.pem" } end }
           term.command{ name = "b.bad", run = function() term.trust{ name = "../x", path = "/c.pem" } end }
           term.notify(term.os)"#,
        Permissions::from_names(&["trust"]),
    )
    .unwrap();
    assert_eq!(notes(&mut host), [std::env::consts::OS]);
    host.run_command("b.trust").unwrap();
    assert_eq!(
        drain(&mut host),
        [Request::Trust {
            name: "dev".into(),
            path: "/c.pem".into(),
            worktree: None,
            done: None
        }]
    );
    assert!(host.run_command("b.bad").is_err());
}

#[test]
fn cache_dirs_are_per_extension() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = ExtensionHost::with_config(Config {
        cache_dir: Some(dir.path().join("extensions")),
        ..Config::default()
    });
    host.load_source("keep", "term.notify(term.cache_dir())")
        .unwrap();
    let expected = dir.path().join("extensions/keep");
    assert_eq!(notes(&mut host), [expected.to_string_lossy().into_owned()]);
    assert!(expected.is_dir());
    let mut host = ExtensionHost::new();
    assert!(host.load_source("none", "term.cache_dir()").is_err());
}

#[test]
fn missing_directory_is_not_an_error() {
    let mut host = ExtensionHost::new();
    assert!(
        host.load_dir(&PathBuf::from("/nonexistent/cmdterm-ext"))
            .is_empty()
    );
}

#[test]
fn events_reach_handlers_with_their_fields() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "events",
        r#"
        local function show(v) return v == nil and "nil" or tostring(v) end
        term.on("app_started", function(e) term.notify("started " .. e.event) end)
        term.on("project_added", function(e)
            term.notify(table.concat({ e.name, e.root, e.env }, "|"))
        end)
        term.on("worktree_activated", function(e)
            term.notify(table.concat({ e.project, e.path, show(e.branch), e.env }, "|"))
        end)
        term.on("terminal_opened", function(e)
            term.notify(math.type(e.id) .. " " .. show(e.id) .. " " .. show(e.cwd))
        end)
        term.on("terminal_exited", function(e)
            term.notify(show(e.id) .. " " .. show(e.code))
        end)
        "#,
    )
    .unwrap();

    let events = [
        Event::AppStarted,
        Event::ProjectAdded {
            name: "cmdterm".into(),
            root: "/src/cmdterm".into(),
            env: "nix".into(),
            project_id: "cmdterm-1".into(),
        },
        Event::WorktreeActivated {
            project: "cmdterm".into(),
            project_id: "cmdterm-1".into(),
            path: "/src/cmdterm-wt".into(),
            branch: Some("feature".into()),
            env: "host".into(),
        },
        Event::WorktreeActivated {
            project: "cmdterm".into(),
            project_id: "cmdterm-1".into(),
            path: "/src/detached".into(),
            branch: None,
            env: "host".into(),
        },
        Event::TerminalOpened {
            id: 7,
            cwd: Some("/tmp".into()),
        },
        Event::TerminalOpened { id: 8, cwd: None },
        Event::TerminalExited {
            id: 7,
            code: Some(-1),
        },
        Event::TerminalExited { id: 8, code: None },
    ];
    for event in &events {
        assert!(host.emit(event).is_empty());
    }
    assert_eq!(
        notes(&mut host),
        [
            "started app_started",
            "cmdterm|/src/cmdterm|nix",
            "cmdterm|/src/cmdterm-wt|feature|host",
            "cmdterm|/src/detached|nil|host",
            "integer 7 /tmp",
            "integer 8 nil",
            "7 -1",
            "8 nil",
        ]
    );
}

#[test]
fn handlers_run_in_load_order() {
    let mut host = ExtensionHost::new();
    for name in ["first", "second", "third"] {
        host.load_source(
            name,
            &format!(
                "term.on('app_started', function() term.notify('{name} a') end)
                 term.on('app_started', function() term.notify('{name} b') end)"
            ),
        )
        .unwrap();
    }
    assert!(host.emit(&Event::AppStarted).is_empty());
    assert_eq!(
        notes(&mut host),
        [
            "first a", "first b", "second a", "second b", "third a", "third b"
        ]
    );
}

#[test]
fn commands_register_and_run() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "palette",
        r#"
        local count = 0
        term.command { name = "palette.count", title = "Count", run = function()
            count = count + 1
            term.notify("count " .. count)
        end }
        term.command { name = "palette.untitled", run = function() end }
        "#,
    )
    .unwrap();
    let commands = host.commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "palette.count");
    assert_eq!(commands[0].title, "Count");
    assert_eq!(commands[0].extension, "palette");
    assert_eq!(commands[1].title, "palette.untitled");

    host.run_command("palette.count").unwrap();
    host.run_command("palette.count").unwrap();
    assert_eq!(notes(&mut host), ["count 1", "count 2"]);

    let error = host.run_command("missing").unwrap_err();
    assert_eq!(error.extension, "scriptschnell-term");
    assert!(enabled(&host, "palette"));
}

#[test]
fn commands_carry_their_key_requests() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "keys",
        r#"
        term.command { name = "one", keys = "ctrl+alt+g", run = function() end }
        term.command { name = "two", run = function() end,
            keys = { default = "ctrl+alt+h", tmux = "ctrl+b h" } }
        "#,
    )
    .unwrap();
    let commands = host.commands();
    assert_eq!(
        commands[0].keys,
        [KeyRequest {
            keymap: None,
            keys: "ctrl+alt+g".into()
        }]
    );
    assert_eq!(
        commands[1].keys,
        [
            KeyRequest {
                keymap: None,
                keys: "ctrl+alt+h".into()
            },
            KeyRequest {
                keymap: Some("tmux".into()),
                keys: "ctrl+b h".into()
            },
        ]
    );
    assert!(commands.iter().all(|c| c.extension == "keys"));
    let revision = host.commands_revision();
    assert_ne!(revision, 0);

    let error = host
        .load_source(
            "bad",
            "term.command{ name = 'b', keys = 3, run = function() end }",
        )
        .unwrap_err();
    assert!(error.message.contains("'keys'"), "{error}");
    assert_eq!(host.commands_revision(), revision, "nothing registered");
}

#[test]
fn a_command_name_cannot_be_taken_from_another_extension() {
    let mut host = ExtensionHost::new();
    host.load_source("one", "term.command{ name = 'x', run = function() end }")
        .unwrap();
    let error = host
        .load_source("two", "term.command{ name = 'x', run = function() end }")
        .unwrap_err();
    assert!(error.message.contains("already registered"), "{error}");
    assert_eq!(host.commands().len(), 1);
    assert_eq!(host.commands()[0].extension, "one");
}

#[test]
fn requests_keep_call_order_across_extensions() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "a",
        r#"
        term.on("app_started", function()
            term.open_terminal { cwd = "/work", cmd = { "cargo", "test" }, title = "tests" }
            term.open_browser("https://example.org")
        end)
        "#,
    )
    .unwrap();
    host.load_source(
        "b",
        r#"
        term.on("app_started", function()
            term.notify("careful", "warning")
            term.send_text("ls\n", 3)
            term.send_text("pwd\n")
            term.switch_worktree("/work/wt")
            term.open_terminal()
        end)
        "#,
    )
    .unwrap();
    assert!(host.emit(&Event::AppStarted).is_empty());
    assert_eq!(
        drain(&mut host),
        [
            Request::OpenTerminal {
                cwd: Some("/work".into()),
                command: Some(vec!["cargo".into(), "test".into()]),
                title: Some("tests".into()),
            },
            Request::OpenBrowser {
                url: "https://example.org".into()
            },
            Request::Notify {
                level: NotifyLevel::Warning,
                message: "careful".into()
            },
            Request::SendText {
                terminal: Some(3),
                text: "ls\n".into()
            },
            Request::SendText {
                terminal: None,
                text: "pwd\n".into()
            },
            Request::SwitchWorktree {
                path: "/work/wt".into()
            },
            Request::OpenTerminal {
                cwd: None,
                command: None,
                title: None
            },
        ]
    );
    assert!(drain(&mut host).is_empty());
}

#[test]
fn a_syntax_error_does_not_affect_other_extensions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("good.lua"),
        "term.on('app_started', function() term.notify('good') end)",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("bad.lua"),
        "term.on('app_started', function()",
    )
    .unwrap();
    let mut host = ExtensionHost::new();
    let errors = host.load_dir(dir.path());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].extension, "bad");
    assert!(errors[0].to_string().starts_with("extension bad: "));
    assert!(!enabled(&host, "bad"));
    assert!(enabled(&host, "good"));
    let bad = host.extensions().find(|ext| ext.name == "bad").unwrap();
    assert!(bad.last_error.is_some());
    assert!(host.emit(&Event::AppStarted).is_empty());
    assert_eq!(notes(&mut host), ["good"]);

    // A fixed version can be loaded again under the same name.
    host.load_source("bad", "term.notify('fixed')").unwrap();
    assert!(enabled(&host, "bad"));
    assert_eq!(host.extensions().count(), 2);
}

#[test]
fn a_failing_handler_disables_only_its_extension() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "flaky",
        r#"
        term.command { name = "flaky.cmd", run = function() end }
        term.on("terminal_opened", function(e) error("no terminal " .. e.id) end)
        "#,
    )
    .unwrap();
    host.load_source(
        "steady",
        "term.on('terminal_opened', function(e) term.notify('steady ' .. e.id) end)",
    )
    .unwrap();
    assert_eq!(host.commands().len(), 1);

    let errors = host.emit(&Event::TerminalOpened { id: 1, cwd: None });
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].extension, "flaky");
    assert!(errors[0].message.contains("no terminal 1"), "{}", errors[0]);
    assert!(!enabled(&host, "flaky"));
    assert!(enabled(&host, "steady"));
    assert!(host.commands().is_empty());
    assert!(host.run_command("flaky.cmd").is_err());

    assert!(
        host.emit(&Event::TerminalOpened { id: 2, cwd: None })
            .is_empty()
    );
    assert_eq!(notes(&mut host), ["steady 1", "steady 2"]);
}

#[test]
fn a_failing_command_disables_its_extension() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "boom",
        "term.command{ name = 'boom', run = function() local t = nil; return t.x end }",
    )
    .unwrap();
    let error = host.run_command("boom").unwrap_err();
    assert_eq!(error.extension, "boom");
    assert!(!enabled(&host, "boom"));
}

#[test]
fn context_reflects_the_last_snapshot() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "ctx",
        r#"
        term.command { name = "ctx.show", run = function()
            local c = term.context()
            term.notify(tostring(c.project) .. "|" .. tostring(c.worktree) .. "|"
                .. tostring(c.branch) .. "|" .. c.env)
        end }
        "#,
    )
    .unwrap();
    host.run_command("ctx.show").unwrap();
    host.set_context(Context {
        project: Some("cmdterm".into()),
        project_id: Some("cmdterm-1".into()),
        worktree: Some("/src/cmdterm".into()),
        branch: Some("main".into()),
        env: "nix".into(),
        runtime: "host".into(),
    });
    host.run_command("ctx.show").unwrap();
    assert_eq!(
        notes(&mut host),
        ["nil|nil|nil|", "cmdterm|/src/cmdterm|main|nix"]
    );
}

#[test]
fn environment_ready_handlers_copy_and_run() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "provision",
        r#"
        term.on("environment_ready", function(e)
            if e.kind ~= "devcontainer" or not e.fresh then return end
            term.copy{ from = "/home/me/.config/tool", to = "~/.config",
                          worktree = e.worktree, exclude = { "*.log" } }
            term.run{ cmd = { "tool", "init" }, worktree = e.worktree }
            term.notify(e.project .. " " .. e.env)
        end)
        "#,
    )
    .unwrap();
    let ready = |fresh| Event::EnvironmentReady {
        project: "app".into(),
        worktree: "/src/app".into(),
        kind: "devcontainer".into(),
        env: "cmdterm-app-1".into(),
        host: "local".into(),
        fresh,
    };
    assert!(host.emit(&ready(false)).is_empty());
    assert!(drain(&mut host).is_empty());
    assert!(host.emit(&ready(true)).is_empty());
    assert_eq!(
        drain(&mut host),
        [
            Request::Copy {
                from: "/home/me/.config/tool".into(),
                to: "~/.config".into(),
                worktree: Some("/src/app".into()),
                exclude: vec!["*.log".into()],
                done: None,
            },
            Request::Run {
                command: vec!["tool".into(), "init".into()],
                worktree: Some("/src/app".into()),
                cwd: None,
                on_host: false,
                done: None,
            },
            Request::Notify {
                level: NotifyLevel::Info,
                message: "app cmdterm-app-1".into(),
            },
        ]
    );
}

#[test]
fn copy_and_run_check_their_arguments() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "bad",
        r#"term.command { name = "bad.copy", run = function() term.copy{ to = "~" } end }
           term.command { name = "bad.run", run = function() term.run{ cmd = {} } end }"#,
    )
    .unwrap();
    let error = host.run_command("bad.copy").unwrap_err();
    assert!(error.message.contains("'from' is required"), "{error}");
}

#[test]
fn bad_arguments_are_lua_errors() {
    let mut host = ExtensionHost::new();
    for (name, source) in [
        ("e1", "term.on('no_such_event', function() end)"),
        ("e2", "term.on('app_started', 42)"),
        ("e3", "term.command{ title = 'x', run = function() end }"),
        ("e4", "term.notify('x', 'loud')"),
        ("e5", "term.send_text('x', -1)"),
        ("e6", "term.open_terminal{ cmd = { {} } }"),
    ] {
        assert!(host.load_source(name, source).is_err(), "{name}");
    }
    let ok = r#"
        local ok, err = pcall(term.open_browser)
        assert(not ok and err:find("string"), err)
        assert(type(term.version) == "string")
    "#;
    host.load_source("ok", ok).unwrap();
    assert!(drain(&mut host).is_empty());
}

#[test]
fn full_stdlib_without_ffi() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "libs",
        r#"
        assert(_VERSION == "Lua 5.4", _VERSION)
        assert(io and os and require and debug and package)
        assert(ffi == nil and package.loaded.ffi == nil)
        assert(not pcall(require, "ffi"))
        "#,
    )
    .unwrap();

    let mut jit = ExtensionHost::with_config(Config {
        profile: crate::ext::CompatibilityProfile::LuaJit21,
        ..Config::default()
    });
    jit.load_source("jit", "assert(ffi == nil) assert(bit and jit)")
        .unwrap();
}

#[test]
fn handlers_survive_garbage_collection() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "gc",
        r#"
        term.on("app_started", function() term.notify("alive") end)
        term.command { name = "gc.churn", run = function()
            for i = 1, 20000 do local t = { i, tostring(i) } end
            collectgarbage("collect")
            collectgarbage("collect")
        end }
        "#,
    )
    .unwrap();
    host.run_command("gc.churn").unwrap();
    assert!(host.emit(&Event::AppStarted).is_empty());
    assert_eq!(notes(&mut host), ["alive"]);
}

#[test]
fn runaway_scripts_hit_the_time_budget() {
    let mut host = ExtensionHost::with_config(Config {
        time_budget: Some(Duration::from_millis(50)),
        ..Config::default()
    });
    let started = Instant::now();
    let error = host.load_source("spin", "while true do end").unwrap_err();
    assert!(error.message.contains("time budget"), "{error}");
    host.load_source(
        "sneaky",
        r#"
        term.on("app_started", function()
            while true do pcall(function() while true do end end) end
        end)
        "#,
    )
    .unwrap();
    host.load_source(
        "resumer",
        r#"
        term.on("app_started", function()
            while true do coroutine.resume(coroutine.create(function() while true do end end)) end
        end)
        "#,
    )
    .unwrap();
    host.load_source(
        "fine",
        "term.on('app_started', function() term.notify('fine') end)",
    )
    .unwrap();
    let errors = host.emit(&Event::AppStarted);
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(errors[0].extension, "sneaky");
    assert_eq!(errors[1].extension, "resumer");
    assert!(errors[1].message.contains("time budget"), "{}", errors[1]);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!enabled(&host, "spin"));
    assert!(!enabled(&host, "sneaky"));
    assert_eq!(notes(&mut host), ["fine"]);
    // A stale interrupt must not hit the next call.
    for _ in 0..20 {
        assert!(host.emit(&Event::AppStarted).is_empty());
    }
}

#[test]
fn os_exit_disables_instead_of_exiting() {
    let mut host = ExtensionHost::new();
    assert!(host.load_source("quitter", "os.exit(1)").is_err());
    assert!(!enabled(&host, "quitter"));
}

#[test]
fn prefix_writer_prefixes_whole_lines() {
    let mut sink = Vec::new();
    {
        let mut writer = PrefixWriter::new("[ext:x] ", &mut sink);
        writer.write_all(b"a").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"b\nc\n\nd").unwrap();
    }
    assert_eq!(
        String::from_utf8(sink).unwrap(),
        "[ext:x] ab\n[ext:x] c\n[ext:x] \n[ext:x] d\n"
    );
}

#[test]
fn print_does_not_fail_the_script() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "printer",
        "print('hello', 1, nil, {}) io.write('partial') io.stderr:write('err\\n')",
    )
    .unwrap();
}

/// Run with `cargo test --release -p cmdterm -- --ignored --nocapture vm_creation_cost`.
#[test]
#[ignore = "measurement, not a check"]
fn vm_creation_cost() {
    const N: u32 = 200;
    let rss = || {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
            * 4096
    };
    let rss_before = rss();
    let started = Instant::now();
    let mut host = ExtensionHost::new();
    for index in 0..N {
        host.load_source(
            &format!("ext{index}"),
            "term.on('app_started', function() end)",
        )
        .unwrap();
    }
    let per_vm = started.elapsed() / N;
    let rss_per_vm = (rss() - rss_before) / u64::from(N);
    let started = Instant::now();
    for _ in 0..1000 {
        host.emit(&Event::AppStarted);
    }
    let per_emit = started.elapsed() / 1000;
    let started = Instant::now();
    for _ in 0..N {
        drop(
            oximond::Lua::builder()
                .profile(crate::ext::CompatibilityProfile::Lua54)
                .build()
                .unwrap(),
        );
    }
    let bare = started.elapsed() / N;
    println!(
        "load_source per extension: {per_vm:?}, ~{} KiB RSS; bare Lua::build: {bare:?}; emit to {N} handlers: {per_emit:?}",
        rss_per_vm / 1024
    );
}

#[test]
fn bundled_extensions_load_and_answer_environment_ready() {
    // Copies, so their manifests apply as they do in the app.
    let dir = tempfile::tempdir().unwrap();
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    for name in ["claude-code", "dotfiles"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        for file in ["init.lua", "manifest.toml"] {
            std::fs::copy(
                shipped.join(name).join(file),
                dir.path().join(name).join(file),
            )
            .unwrap();
        }
    }
    let mut host = ExtensionHost::new();
    assert!(host.load_dir(dir.path()).is_empty());
    let ready = |kind: &str| Event::EnvironmentReady {
        project: "app".into(),
        worktree: "/src/app".into(),
        kind: kind.into(),
        env: "box".into(),
        host: "local".into(),
        fresh: true,
    };
    // Plain SSH hosts are left alone by default.
    assert!(host.emit(&ready("ssh")).is_empty());
    assert!(drain(&mut host).is_empty());
    assert!(host.emit(&ready("devcontainer")).is_empty());
    for request in drain(&mut host) {
        match request {
            Request::Copy { to, worktree, .. } => {
                assert!(to.starts_with('~'), "{to}");
                assert_eq!(worktree.as_deref(), Some(std::path::Path::new("/src/app")));
            }
            Request::Run { command, .. } => assert_eq!(command[0], "sh"),
            Request::Notify { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    let names: Vec<_> = host.commands().iter().map(|c| c.name.clone()).collect();
    assert!(names.contains(&"claude-code.setup".to_owned()), "{names:?}");
    assert!(names.contains(&"dotfiles.copy".to_owned()), "{names:?}");
}

#[test]
fn sandbox_rules_merge_across_extensions_and_leave_with_a_failed_one() {
    let mut host = ExtensionHost::new();
    let start = host.sandbox_revision();
    host.load_source(
        "npm",
        r#"term.sandbox{
            hosts = { "registry.npmjs.org", "*.npmjs.com" },
            writable = { "~/.npm" },
            redirects = { { host = "~/.cache/npm-sandbox", sandbox = "~/.npmrc.d" } },
        }"#,
    )
    .unwrap();
    host.load_source(
        "keys",
        r#"term.sandbox{ hosts = { "registry.npmjs.org" }, hidden = { "~/.vault" } }
           term.command{ name = "keys.fail", run = function() error("boom") end }"#,
    )
    .unwrap();
    assert_ne!(host.sandbox_revision(), start);
    let rules = host.sandbox_rules();
    assert_eq!(rules.hosts, ["registry.npmjs.org", "*.npmjs.com"]);
    assert_eq!(rules.writable, ["~/.npm"]);
    assert_eq!(rules.hidden, ["~/.vault"]);
    assert_eq!(
        rules.redirects,
        [("~/.cache/npm-sandbox".to_owned(), "~/.npmrc.d".to_owned())]
    );

    // A failed extension's rules go away with it.
    let before = host.sandbox_revision();
    assert!(host.run_command("keys.fail").is_err());
    assert_ne!(host.sandbox_revision(), before);
    assert!(host.sandbox_rules().hidden.is_empty());
    assert_eq!(host.sandbox_rules().hosts.len(), 2);

    // Bad input is an error that disables the extension.
    let error = host
        .load_source("bad", r#"term.sandbox{ hosts = { "http://x/" } }"#)
        .unwrap_err();
    assert!(error.message.contains("not a host name"), "{error}");
    let error = host
        .load_source("bad2", r#"term.sandbox{ redirects = { { host = "/a" } } }"#)
        .unwrap_err();
    assert!(error.message.contains("'host' and 'sandbox'"), "{error}");
}

#[test]
fn secrets_files_vars_and_guards_reach_the_rules() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "npm",
        r#"
        local fake = term.secret{ value = "npm_0123456789abcdefghijKLMNOP", hosts = { "registry.npmjs.org" } }
        assert(fake ~= "npm_0123456789abcdefghijKLMNOP")
        assert(fake == term.secret{ value = "npm_0123456789abcdefghijKLMNOP", hosts = { "*.npmjs.org" } })
        FAKE = fake
        term.sandbox{
          vars = { NPM_TOKEN = fake },
          files = { { sandbox = "~/.npmrc", content = "//registry.npmjs.org/:_authToken=" .. fake } },
          guards = { { name = "publish", title = "npm publish", hosts = { "registry.npmjs.org" }, requests = { "PUT /*" } } },
        }
        "#,
    )
    .unwrap();
    host.load_source(
        "git",
        r#"term.sandbox{ vars = { NPM_TOKEN = "mine" }, guards = {
            { name = "push", hosts = { "*" }, requests = { "POST */git-receive-pack" } } } }"#,
    )
    .unwrap();
    let rules = host.sandbox_rules();
    assert_eq!(rules.secrets.len(), 1);
    let secret = &rules.secrets[0];
    assert_eq!(secret.real, "npm_0123456789abcdefghijKLMNOP");
    assert_eq!(secret.hosts, ["registry.npmjs.org", "*.npmjs.org"]);
    assert!(!format!("{rules:?}").contains("npm_0123456789abcdefghijKLMNOP"));
    // The first extension keeps a variable.
    assert_eq!(rules.vars, [("NPM_TOKEN".to_owned(), secret.fake.clone())]);
    assert_eq!(
        rules.files,
        [(
            "~/.npmrc".to_owned(),
            format!("//registry.npmjs.org/:_authToken={}", secret.fake)
        )]
    );
    let ids: Vec<&str> = rules.guards.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["npm.publish", "git.push"]);
    assert_eq!(rules.guards[1].title, "push");

    // A later `term.sandbox` keeps the secrets.
    let before = host.sandbox_revision();
    host.load_source("other", "term.sandbox{}").unwrap();
    assert_eq!(host.sandbox_revision(), before);

    for (source, message) in [
        (r#"term.secret{ value = "x" }"#, "'hosts'"),
        (
            r#"term.secret{ value = "", hosts = { "a.io" } }"#,
            "'value'",
        ),
        (
            r#"term.secret{ value = "x", hosts = { "*" } }"#,
            "not a host name",
        ),
        (
            r#"term.sandbox{ guards = { { name = "a b", hosts = { "*" }, requests = { "PUT /" } } } }"#,
            "'name'",
        ),
        (
            r#"term.sandbox{ guards = { { name = "a", hosts = { "*" }, requests = { "PUT" } } } }"#,
            "not a request",
        ),
        (
            r#"term.sandbox{ guards = { { name = "a", requests = { "PUT /" } } } }"#,
            "needs 'hosts'",
        ),
        (
            r#"term.sandbox{ vars = { ["A=B"] = "x" } }"#,
            "not a variable name",
        ),
        (
            r#"term.sandbox{ files = { { sandbox = "~/.x" } } }"#,
            "'content'",
        ),
    ] {
        let error = host.load_source("bad", source).unwrap_err();
        assert!(error.message.contains(message), "{source}: {error}");
    }
}

#[test]
fn blocked_hosts_and_blocklists_reach_the_rules() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "ads",
        r#"term.sandbox{
            blocked = { "ads.example", "*.track.example" },
            unblocked = { "ok.ads.example" },
            blocklists = {
              { name = "big", urls = { "https://a.example/l.txt", "https://b.example/l.txt" } },
              { name = "daily", urls = { "https://c.example/l.txt" }, max_age = 86400 },
            },
        }"#,
    )
    .unwrap();
    host.load_source(
        "more",
        r#"term.sandbox{ blocked = { "ads.example", "x.example" } }"#,
    )
    .unwrap();
    let rules = host.sandbox_rules();
    assert_eq!(
        rules.blocked,
        ["ads.example", "*.track.example", "x.example"]
    );
    assert_eq!(rules.unblocked, ["ok.ads.example"]);
    assert_eq!(
        rules.blocklists,
        [
            crate::ext::BlocklistSource {
                id: "ads.big".into(),
                urls: vec![
                    "https://a.example/l.txt".into(),
                    "https://b.example/l.txt".into()
                ],
                max_age: 7 * 24 * 60 * 60,
            },
            crate::ext::BlocklistSource {
                id: "ads.daily".into(),
                urls: vec!["https://c.example/l.txt".into()],
                max_age: 86400,
            },
        ]
    );

    for (source, message) in [
        (r#"term.sandbox{ blocked = { "a b" } }"#, "not a host name"),
        (
            r#"term.sandbox{ blocklists = { { name = "a", urls = { "http://a.example/" } } } }"#,
            "not an https:// URL",
        ),
        (
            r#"term.sandbox{ blocklists = { { name = "a" } } }"#,
            "needs 'urls'",
        ),
        (
            r#"term.sandbox{ blocklists = { { name = "a", urls = { "https://a.example/" }, max_age = 0 } } }"#,
            "'max_age'",
        ),
        (
            r#"term.sandbox{ blocklists = { { name = "a.b", urls = { "https://a.example/" } } } }"#,
            "'name'",
        ),
    ] {
        let error = host.load_source("bad", source).unwrap_err();
        assert!(error.message.contains(message), "{source}: {error}");
    }
}

#[test]
fn the_blocklist_extension_names_hagezi_with_a_mirror() {
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("blocklist")).unwrap();
    for file in ["init.lua", "manifest.toml"] {
        std::fs::copy(
            shipped.join("blocklist").join(file),
            dir.path().join("blocklist").join(file),
        )
        .unwrap();
    }
    let mut host = ExtensionHost::new();
    assert!(host.load_dir(dir.path()).is_empty());
    let lists = &host.sandbox_rules().blocklists;
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].id, "blocklist.hagezi-pro");
    assert_eq!(lists[0].max_age, 7 * 24 * 60 * 60);
    assert!(lists[0].urls[0].starts_with("https://raw.githubusercontent.com/hagezi/"));
    assert!(lists[0].urls[1].starts_with("https://codeberg.org/hagezi/"));
}

#[test]
fn the_registry_extensions_give_sandboxes_fakes_and_guard_private_registries() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let write = |path: &str, text: &str| {
        let path = home.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        ".npmrc",
        "registry=https://registry.npmjs.org/\n\
         //registry.npmjs.org/:_authToken=npm_REALREALREALREALREALREAL0123\n\
         //npm.corp.example/:_password=c2VjcmV0LXBhc3N3b3Jk\n\
         //env.example/:_authToken=${NPM_TOKEN}\n",
    );
    write(
        ".cargo/credentials.toml",
        "[registry]\ntoken = \"cioREALREALREALREALREALREALREAL\"\n\
         [registries.corp]\ntoken = \"corp-real-token-0123456789\"\n\
         [registries.lost]\ntoken = \"lost-real-token-0123456789\"\n",
    );
    write(
        ".cargo/config.toml",
        "[registries.corp]\nindex = \"sparse+https://cargo.corp.example/index/\"\n",
    );
    write(
        ".nuget/NuGet/NuGet.Config",
        r#"<configuration>
  <packageSources>
    <add key="Corp Feed" value="https://nuget.corp.example/v3/index.json" />
  </packageSources>
  <packageSourceCredentials>
    <Corp_x0020_Feed>
      <add key="Username" value="me" />
      <add key="ClearTextPassword" value="nuget-real-password" />
    </Corp_x0020_Feed>
  </packageSourceCredentials>
</configuration>"#,
    );
    write(
        ".pypirc",
        "[distutils]\nindex-servers = pypi\n[pypi]\nusername = __token__\npassword = pypi-REALREALREAL\n",
    );
    write(
        ".git-credentials",
        "https://me:ghp_REALREALREALREALREAL0123@github.com\n",
    );
    let stub = format!(
        "local os_getenv = function(name) if name == 'HOME' then return {:?} end return nil end\n",
        home.to_str().unwrap()
    );
    let mut host = ExtensionHost::new();
    for (name, source) in [
        ("npm", include_str!("../../../../extensions/npm/init.lua")),
        (
            "cargo",
            include_str!("../../../../extensions/cargo/init.lua"),
        ),
        (
            "nuget",
            include_str!("../../../../extensions/nuget/init.lua"),
        ),
        ("pypi", include_str!("../../../../extensions/pypi/init.lua")),
        (
            "git-credentials",
            include_str!("../../../../extensions/git-credentials/init.lua"),
        ),
    ] {
        let source = source.replace("os.getenv(", "os_getenv(");
        host.load_source(name, &(stub.clone() + &source)).unwrap();
        assert!(drain(&mut host).is_empty(), "{name} reported a problem");
    }
    let rules = host.sandbox_rules();

    let reals = [
        "npm_REALREALREALREALREALREAL0123",
        "secret-password",
        "cioREALREALREALREALREALREALREAL",
        "corp-real-token-0123456789",
        "nuget-real-password",
        "pypi-REALREALREAL",
        "ghp_REALREALREALREALREAL0123",
    ];
    for (path, content) in &rules.files {
        for real in reals
            .iter()
            .chain(&["lost-real-token", "c2VjcmV0LXBhc3N3b3Jk"])
        {
            assert!(!content.contains(real), "{path} shows {real}:\n{content}");
        }
    }
    let secret_hosts = |real: &str| {
        rules
            .secrets
            .iter()
            .find(|s| s.real == real)
            .map(|s| s.hosts.clone())
            .unwrap_or_else(|| panic!("no secret for {real}"))
    };
    assert_eq!(secret_hosts(reals[0]), ["registry.npmjs.org"]);
    assert_eq!(secret_hosts(reals[1]), ["npm.corp.example"]);
    assert_eq!(secret_hosts(reals[2]), ["crates.io"]);
    assert_eq!(secret_hosts(reals[3]), ["cargo.corp.example"]);
    assert_eq!(secret_hosts(reals[4]), ["nuget.corp.example"]);
    assert_eq!(secret_hosts(reals[5]), ["upload.pypi.org"]);
    assert_eq!(secret_hosts(reals[6]), ["github.com"]);
    assert_eq!(rules.secrets.len(), reals.len());

    let file = |name: &str| {
        rules
            .files
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .map(|(_, content)| content.clone())
            .unwrap()
    };
    let npmrc = file(".npmrc");
    let fake = &rules
        .secrets
        .iter()
        .find(|s| s.real == reals[0])
        .unwrap()
        .fake;
    assert!(
        npmrc.contains(&format!("//registry.npmjs.org/:_authToken={fake}")),
        "{npmrc}"
    );
    assert!(npmrc.contains("${NPM_TOKEN}"), "{npmrc}");
    assert!(file("credentials.toml").contains("[registries.lost]\ntoken = \"\""));
    assert!(file("NuGet.Config").contains(r#"<add key="Username" value="me" />"#));

    // Public registries have the proxy's own guards; extensions guard the
    // private ones they found.
    let guards: Vec<(&str, Vec<String>)> = rules
        .guards
        .iter()
        .map(|g| (g.id.as_str(), g.hosts.clone()))
        .collect();
    let list = |hosts: &[&str]| hosts.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        guards,
        [
            ("npm.publish", list(&["npm.corp.example", "env.example"])),
            ("cargo.publish", list(&["cargo.corp.example"])),
            ("nuget.push", list(&["nuget.corp.example"])),
        ]
    );
    assert!(rules.hosts.contains(&"crates.io".to_owned()));
    assert!(rules.writable.contains(&"~/.npm".to_owned()));
}

#[test]
fn forge_clis_give_sandboxes_fakes_and_refresh_oauth_logins_here() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let write = |path: &str, text: &str| {
        let path = home.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let keys = |token: &str, year: u32| {
        format!(
            r#"{{"hosts":{{"git.example.org":{{"type":"OAuth","token":"{token}",
            "refresh_token":"fj-real-refresh-0123456789abcdef",
            "expires_at":[{year},302,19,18,19,487912501,0,0,0]}},
            "codeberg.org":{{"type":"Application","token":"0123456789abcdef0123456789abcdef01234567"}}}},
            "aliases":{{}},"default_ssh":[]}}"#
        )
    };
    let keys_path = ".local/share/forgejo-cli/keys.json";
    write(
        keys_path,
        &keys("fj-first-access-token-0123456789abcdef", 2020),
    );
    write(
        ".config/tea/config.yml",
        "logins:\n    - name: home\n      url: https://tea.example.org\n      token: tea-real-token-0123456789abcdef\n      default: true\n    - name: work\n      url: https://tea-oauth.example.org\n      token: tea-oauth-token-0123456789abcdef\n      refresh_token: tea-real-refresh-0123456789abcdef\n      token_expiry: 1577836800\npreferences:\n    editor: false\n",
    );
    write(
        ".config/glab-cli/config.yml",
        "host: gitlab.com\nhosts:\n    gitlab.com:\n        token:\n        api_host: gitlab.com\n    lab.example.org:\n        token: glpat-REALREALREALREALREAL\n        api_host: api.lab.example.org\n        user: me\n    oauth.example.org:\n        is_oauth2: \"true\"\n        oauth2_refresh_token: glab-real-refresh-0123456789abcdef\n        oauth2_expiry_date: 2020-01-01T09:00:00+01:00\n        token: glab-oauth-token-0123456789abcdef\n",
    );
    write(
        ".config/gh/hosts.yml",
        "github.com:\n    git_protocol: https\n    users:\n        me:\n    user: me\n",
    );
    let stub = format!(
        "local os_getenv = function(name) if name == 'HOME' then return {:?} end return nil end\n",
        home.to_str().unwrap()
    );
    let source = include_str!("../../../../extensions/forge-clis/init.lua")
        .replace("os.getenv(", "os_getenv(");
    let mut host = ExtensionHost::new();
    host.load_source("forge-clis", &(stub + &source)).unwrap();
    assert!(drain(&mut host).is_empty());

    let file = |host: &ExtensionHost, name: &str| {
        let rules = host.sandbox_rules();
        let (_, content) = rules
            .files
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .unwrap_or_else(|| panic!("no {name} in {:?}", rules.files));
        content.clone()
    };
    let fake = |host: &ExtensionHost, real: &str| {
        let rules = host.sandbox_rules();
        let secret = rules
            .secrets
            .iter()
            .find(|s| s.real == real)
            .unwrap_or_else(|| panic!("no secret for {real}"));
        (secret.fake.clone(), secret.hosts.clone())
    };
    let json = file(&host, "keys.json");
    let (fj_fake, hosts) = fake(&host, "fj-first-access-token-0123456789abcdef");
    assert_eq!(hosts, ["git.example.org"]);
    for real in [
        "fj-first-access-token",
        "fj-real-refresh",
        "0123456789abcdef0123",
    ] {
        assert!(!json.contains(real), "{json}");
    }
    assert!(json.contains(&fj_fake), "{json}");
    assert!(json.contains("[9999,1,0,0,0,0,0,0,0]"), "{json}");
    assert!(json.contains(r#""aliases":{}"#), "{json}");
    assert!(json.contains(r#""default_ssh":[]"#), "{json}");

    let tea = file(&host, "tea/config.yml");
    let (tea_fake, hosts) = fake(&host, "tea-real-token-0123456789abcdef");
    assert_eq!(hosts, ["tea.example.org"]);
    assert!(tea.contains(&format!("      token: {tea_fake}\n")), "{tea}");
    // OAuth logins in the sandbox never expire and cannot refresh.
    assert!(tea.contains("      token_expiry: 253402300799\n"), "{tea}");
    assert!(!tea.contains("tea-real-refresh"), "{tea}");
    assert!(!tea.contains("tea-oauth-token"), "{tea}");

    let glab = file(&host, "glab-cli/config.yml");
    let (glab_fake, hosts) = fake(&host, "glpat-REALREALREALREALREAL");
    assert_eq!(hosts, ["lab.example.org", "api.lab.example.org"]);
    assert!(
        glab.contains(&format!("        token: {glab_fake}\n")),
        "{glab}"
    );
    assert!(glab.contains("    gitlab.com:\n        token:\n"), "{glab}");
    assert!(
        glab.contains("        oauth2_expiry_date: 9999-12-31T23:59:59Z\n"),
        "{glab}"
    );
    assert!(!glab.contains("glab-real-refresh"), "{glab}");
    assert!(!glab.contains("glab-oauth-token"), "{glab}");
    // Newer tea keeps OAuth tokens in credentials.json, which a fake
    // cannot stand in for.
    assert!(
        host.sandbox_rules()
            .hidden
            .iter()
            .any(|path| path.ends_with("/tea/credentials.json"))
    );
    for allowed in [
        "git.example.org",
        "tea.example.org",
        "api.lab.example.org",
        "api.github.com",
    ] {
        assert!(
            host.sandbox_rules().hosts.contains(&allowed.to_owned()),
            "{allowed}"
        );
    }

    // A sandbox on this machine runs fj, glab and tea for their expired
    // logins and asks gh for its keyring token.
    host.emit(&Event::EnvironmentReady {
        project: "app".into(),
        worktree: "/src/app".into(),
        kind: "sandbox".into(),
        env: "local".into(),
        host: "local".into(),
        fresh: false,
    });
    let mut runs: Vec<(Vec<String>, _)> = drain(&mut host)
        .into_iter()
        .filter_map(|request| match request {
            Request::Run {
                command,
                on_host: true,
                done: Some(done),
                ..
            } => Some((command, done)),
            _ => None,
        })
        .collect();
    runs.sort_by(|a, b| a.0.cmp(&b.0));
    let commands: Vec<&[String]> = runs.iter().map(|(c, _)| c.as_slice()).collect();
    assert_eq!(
        commands,
        [
            ["fj", "whoami", "--host", "https://git.example.org"]
                .map(str::to_owned)
                .as_slice(),
            ["gh", "auth", "token", "--hostname", "github.com"]
                .map(str::to_owned)
                .as_slice(),
            ["glab", "api", "user", "--hostname", "oauth.example.org"]
                .map(str::to_owned)
                .as_slice(),
            [
                "tea", "repos", "list", "--login", "work", "--limit", "1", "--output", "simple"
            ]
            .map(str::to_owned)
            .as_slice(),
        ]
    );
    // glab and tea refresh only near the expiry, so the extension marked
    // the logins expired first.
    let glab_here = std::fs::read_to_string(home.join(".config/glab-cli/config.yml")).unwrap();
    assert!(
        glab_here.contains(
            "        oauth2_expiry_date: 1970-01-01T00:00:00Z\n        token: glab-oauth-token"
        ),
        "{glab_here}"
    );
    assert!(
        glab_here.contains("glab-real-refresh-0123456789abcdef"),
        "{glab_here}"
    );
    let tea_here = std::fs::read_to_string(home.join(".config/tea/config.yml")).unwrap();
    assert!(tea_here.contains("      token_expiry: 1\n"), "{tea_here}");
    assert!(
        tea_here.contains("tea-real-refresh-0123456789abcdef"),
        "{tea_here}"
    );
    let (_, tea_done) = runs.pop().unwrap();
    let (_, glab_done) = runs.pop().unwrap();
    // glab refreshed: the fake now stands for the new token.
    write(
        ".config/glab-cli/config.yml",
        &glab_here
            .replace(
                "glab-oauth-token-0123456789abcdef",
                "glab-oauth-token-fedcba9876543210",
            )
            .replace("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z"),
    );
    let (glab_oauth_fake, _) = fake(&host, "glab-oauth-token-0123456789abcdef");
    host.complete(glab_done, &ok("{}")).unwrap();
    assert_eq!(
        fake(&host, "glab-oauth-token-fedcba9876543210").0,
        glab_oauth_fake
    );
    let (_, gh_done) = runs.pop().unwrap();
    let (_, fj_done) = runs.pop().unwrap();
    // fj refreshes a login only once it has expired, so the extension
    // marked it expired first. Everything else stays.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(keys_path)).unwrap()).unwrap();
    let login = &on_disk["hosts"]["git.example.org"];
    assert_eq!(
        login["expires_at"],
        serde_json::json!([1970, 1, 0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(login["token"], "fj-first-access-token-0123456789abcdef");
    assert_eq!(login["refresh_token"], "fj-real-refresh-0123456789abcdef");
    assert_eq!(
        on_disk["hosts"]["codeberg.org"]["token"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(on_disk["aliases"], serde_json::json!({}));
    assert_eq!(on_disk["default_ssh"], serde_json::json!([]));

    host.complete(gh_done, &ok("gho_REALREALREALREALREALREAL0123\n"))
        .unwrap();
    let (gh_fake, hosts) = fake(&host, "gho_REALREALREALREALREALREAL0123");
    assert!(hosts.contains(&"api.github.com".to_owned()), "{hosts:?}");
    let gh = file(&host, "gh/hosts.yml");
    assert!(
        gh.contains(&format!("    user: me\n    oauth_token: {gh_fake}\n")),
        "{gh}"
    );

    // While fj refreshes, switching worktrees starts no second refresh,
    // which would reuse the refresh token, and gh is asked only once.
    host.emit(&Event::WorktreeActivated {
        project: "app".into(),
        project_id: "app-1".into(),
        path: "/src/app".into(),
        branch: None,
        env: "local".into(),
    });
    assert!(drain(&mut host).is_empty());

    // fj refreshed the login: the fake now stands for the new token.
    write(
        keys_path,
        &keys("fj-second-access-token-0123456789abcdef", 2999),
    );
    host.complete(fj_done, &ok("")).unwrap();
    assert_eq!(file(&host, "keys.json"), json);
    let (refreshed, _) = fake(&host, "fj-second-access-token-0123456789abcdef");
    assert_eq!(refreshed, fj_fake);
    assert!(
        !host
            .sandbox_rules()
            .secrets
            .iter()
            .any(|s| s.real.starts_with("fj-first"))
    );
    assert!(notes(&mut host).is_empty());

    // A failed refresh asks the user to log in again, once.
    host.complete(
        tea_done,
        &JobResult::from_output(Some(1), b"", b"invalid_grant"),
    )
    .unwrap();
    assert_eq!(
        notes(&mut host),
        [
            "forge-clis: tea could not refresh its login for work, so sandboxes cannot use it; \
          run tea login oauth-refresh work on this machine"
        ]
    );

    // A login that does not expire soon is left alone.
    let fresh = std::fs::read_to_string(home.join(keys_path)).unwrap();
    host.emit(&Event::EnvironmentReady {
        project: "app".into(),
        worktree: "/src/app".into(),
        kind: "sandbox".into(),
        env: "local".into(),
        host: "local".into(),
        fresh: false,
    });
    let commands: Vec<Vec<String>> = drain(&mut host)
        .into_iter()
        .filter_map(|request| match request {
            Request::Run { command, .. } => Some(command),
            _ => None,
        })
        .collect();
    // tea's failed login is marked expired already, so it is tried again.
    assert_eq!(
        commands,
        [
            vec![
                "tea", "repos", "list", "--login", "work", "--limit", "1", "--output", "simple"
            ],
            vec!["gh", "auth", "token", "--hostname", "github.com"],
        ]
    );
    assert_eq!(
        std::fs::read_to_string(home.join(keys_path)).unwrap(),
        fresh
    );
}

#[test]
fn extensions_without_a_permission_lose_its_functions_and_libraries() {
    let mut host = ExtensionHost::new();
    host.load_source_with(
        "plain",
        r#"
        assert(io == nil and package.loaded.io == nil and dofile == nil and loadfile == nil)
        assert(os.getenv == nil and os.execute == nil and os.remove == nil)
        assert(debug == nil and package.loaded.debug == nil and package.loadlib == nil)
        assert(package.loaded.os == os and os.time ~= nil)
        assert(not pcall(term.secret, { value = "x", hosts = { "a.io" } }))
        local ok, error = pcall(term.run, { cmd = { "true" } })
        assert(not ok and error:find("'run' permission"), error)
        term.command{ name = "plain.hi", run = function() end }
        term.notify("still here")
        "#,
        Permissions::NONE,
    )
    .unwrap();
    assert_eq!(notes(&mut host), ["still here"]);

    let some: Permissions = [Permission::Env, Permission::Filesystem, Permission::Sandbox]
        .into_iter()
        .collect();
    host.load_source_with(
        "some",
        r#"
        assert(io.open ~= nil and os.getenv ~= nil)
        assert(io.popen == nil and os.execute == nil and debug == nil)
        term.sandbox{ hosts = { "a.io" } }
        local ok, error = pcall(term.sandbox, { guards = {
          { name = "g", hosts = { "*" }, requests = { "PUT /" } } } })
        assert(not ok and error:find("'guards' permission"), error)
        "#,
        some,
    )
    .unwrap();
    assert_eq!(host.sandbox_rules().hosts, ["a.io"]);
}

#[test]
fn manifests_declare_permissions_the_user_must_grant() {
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("npm");
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(
        ext.join("manifest.toml"),
        r#"description = "npm logins"
           permissions = ["env", "secrets"]"#,
    )
    .unwrap();
    std::fs::write(
        ext.join("init.lua"),
        r#"term.notify(term.secret{ value = os.getenv("PATH"), hosts = { "a.io" } } ~= nil and "ok" or "no")"#,
    )
    .unwrap();
    let broken = dir.path().join("broken");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join("manifest.toml"), r#"permissions = ["root"]"#).unwrap();
    std::fs::write(broken.join("init.lua"), "").unwrap();

    let mut granted = std::collections::BTreeMap::new();
    let mut host = ExtensionHost::new();
    let errors = host.load_dir_except(dir.path(), &[], &granted, &Default::default());
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown permission 'root'"))
    );
    let npm = host.extensions().find(|e| e.name == "npm").unwrap();
    assert!(!npm.enabled);
    assert_eq!(npm.missing.names(), ["secrets", "env"]);
    assert_eq!(npm.description.as_deref(), Some("npm logins"));
    assert!(notes(&mut host).is_empty());

    // A partial grant runs it with just those; the rest stay missing.
    granted.insert("npm".to_owned(), vec!["env".to_owned()]);
    let mut host = ExtensionHost::new();
    host.load_dir_except(dir.path(), &[], &granted, &Default::default());
    let npm = host.extensions().find(|e| e.name == "npm").unwrap();
    assert_eq!(npm.missing.names(), ["secrets"]);

    granted.insert(
        "npm".to_owned(),
        vec!["env".to_owned(), "secrets".to_owned()],
    );
    let mut host = ExtensionHost::new();
    host.load_dir_except(dir.path(), &[], &granted, &Default::default());
    let npm = host.extensions().find(|e| e.name == "npm").unwrap();
    assert!(npm.enabled && npm.missing.is_empty());
    assert_eq!(npm.permissions.names(), ["secrets", "env"]);
    assert_eq!(notes(&mut host), ["ok"]);
}

#[test]
fn a_partial_grant_runs_the_extension_with_only_the_granted_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("mixed");
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(
        ext.join("manifest.toml"),
        r#"permissions = ["env", "secrets"]"#,
    )
    .unwrap();
    std::fs::write(
        ext.join("init.lua"),
        r#"
        term.notify("env:" .. tostring(type(os.getenv) == "function"))
        local ok = pcall(term.secret, { value = "x", hosts = { "a.io" } })
        term.notify("secret:" .. tostring(ok))
        "#,
    )
    .unwrap();
    // Only `env` is granted: the extension loads and runs with just `env`.
    let mut granted = std::collections::BTreeMap::new();
    granted.insert("mixed".to_owned(), vec!["env".to_owned()]);
    let mut host = ExtensionHost::new();
    let errors = host.load_dir_except(dir.path(), &[], &granted, &Default::default());
    assert!(errors.is_empty(), "{errors:?}");
    let mixed = host.extensions().find(|e| e.name == "mixed").unwrap();
    assert!(mixed.enabled);
    assert_eq!(mixed.missing.names(), ["secrets"]);
    // `os.getenv` is behind `env`, which is granted; `term.secret` is behind
    // `secrets`, which is not.
    assert_eq!(notes(&mut host), ["env:true", "secret:false"]);
}

#[test]
fn shipped_extensions_declare_what_they_use() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let mut host = ExtensionHost::new();
    host.turn_on(&[
        "aspnet-devcerts".to_owned(),
        "coder".to_owned(),
        "kubernetes".to_owned(),
    ]);
    let errors = host.load_dir(&root);
    assert!(errors.is_empty(), "{errors:?}");
    let names: Vec<String> = host.extensions().map(|e| e.name.clone()).collect();
    assert_eq!(
        names,
        [
            "aspnet-devcerts",
            "azure-devops",
            "bitbucket",
            "blocklist",
            "cargo",
            "claude-code",
            "coder",
            "dotfiles",
            "forge-clis",
            "forgejo",
            "git-credentials",
            "gitea",
            "github",
            "gitlab",
            "knowledge",
            "kubernetes",
            "npm",
            "nuget",
            "pypi",
            "toolchains"
        ]
    );
    for ext in host.extensions() {
        assert!(ext.enabled, "{}: {:?}", ext.name, ext.last_error);
        assert!(ext.description.is_some(), "{} has no description", ext.name);
    }
}

#[test]
fn aspnet_devcerts_is_off_until_turned_on() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let mut host = ExtensionHost::new();
    assert!(host.load_dir(&root).is_empty());
    let ext = host
        .extensions()
        .find(|e| e.name == "aspnet-devcerts")
        .unwrap();
    assert!(ext.turned_off && !ext.default_on);
}

/// Loads the shipped aspnet-devcerts extension with a cache directory.
fn aspnet_host(cache: &std::path::Path) -> ExtensionHost {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let mut host = ExtensionHost::with_config(Config {
        cache_dir: Some(cache.to_path_buf()),
        ..Config::default()
    });
    let grants = std::collections::BTreeMap::from([(
        "aspnet-devcerts".to_owned(),
        ["filesystem", "process", "copy", "run", "trust"]
            .map(str::to_owned)
            .to_vec(),
    )]);
    let off: Vec<String> = [
        "blocklist",
        "cargo",
        "claude-code",
        "dotfiles",
        "forge-clis",
        "forgejo",
        "git-credentials",
        "gitea",
        "github",
        "gitlab",
        "azure-devops",
        "bitbucket",
        "knowledge",
        "npm",
        "nuget",
        "pypi",
        "toolchains",
    ]
    .map(str::to_owned)
    .to_vec();
    host.turn_on(&["aspnet-devcerts".to_owned()]);
    let errors = host.load_dir_except(&root, &off, &grants, &Default::default());
    assert!(errors.is_empty(), "{errors:?}");
    host
}

fn ok(stdout: &str) -> JobResult {
    JobResult::from_output(Some(0), stdout.as_bytes(), b"")
}

#[test]
fn aspnet_devcerts_carries_the_hosts_certificate_into_devcontainers() {
    let cache = tempfile::tempdir().unwrap();
    let mut host = aspnet_host(cache.path());
    let ready = |host: &str| Event::EnvironmentReady {
        project: "api".into(),
        worktree: "/src/api".into(),
        kind: "devcontainer".into(),
        env: "cmdterm-api-1".into(),
        host: host.into(),
        fresh: true,
    };
    // Devcontainers on other machines are left alone.
    assert!(host.emit(&ready("dev-box")).is_empty());
    assert!(drain(&mut host).is_empty());

    assert!(host.emit(&ready("local")).is_empty());
    let next = |host: &mut ExtensionHost| {
        let requests = drain(host);
        assert_eq!(requests.len(), 1, "{requests:?}");
        requests.into_iter().next().unwrap()
    };
    let Request::Run {
        command,
        on_host: false,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no dotnet check");
    };
    assert_eq!(command[..2], ["sh", "-c"]);
    host.complete(done, &ok("/usr/bin/dotnet")).unwrap();

    let Request::Run {
        command,
        on_host: true,
        done: Some(done),
        worktree,
        ..
    } = next(&mut host)
    else {
        panic!("no host create");
    };
    assert_eq!(command, ["dotnet", "dev-certs", "https", "--trust"]);
    assert_eq!(worktree.as_deref(), Some(std::path::Path::new("/src/api")));
    host.complete(done, &ok("A valid HTTPS certificate is already present."))
        .unwrap();

    let Request::Run {
        command,
        on_host: true,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no pfx export");
    };
    assert_eq!(
        command[..4],
        ["dotnet", "dev-certs", "https", "--export-path"]
    );
    let pfx = std::path::PathBuf::from(&command[4]);
    assert!(
        pfx.starts_with(cache.path().join("aspnet-devcerts")),
        "{pfx:?}"
    );
    assert_eq!(command[5], "--password");
    let password = command[6].clone();
    assert_eq!(password.len(), 32);
    std::fs::write(&pfx, "pfx").unwrap();
    host.complete(done, &ok("")).unwrap();

    let Request::Run {
        command,
        on_host: true,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no pem export");
    };
    assert_eq!(command[5..], ["--format", "PEM", "--no-password"]);
    let crt = std::path::PathBuf::from(&command[4]);
    let key = crt.with_extension("key");
    std::fs::write(&crt, "-----BEGIN CERTIFICATE-----").unwrap();
    std::fs::write(&key, "private").unwrap();
    host.complete(done, &ok("")).unwrap();
    assert!(!key.exists(), "the private key stays behind");

    let Request::Copy {
        from,
        to,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no copy");
    };
    assert_eq!(
        (from.as_path(), to.as_str()),
        (pfx.as_path(), "~/.aspnet/https")
    );
    host.complete(
        done,
        &JobResult {
            ok: true,
            ..JobResult::default()
        },
    )
    .unwrap();
    assert!(!pfx.exists(), "the export is deleted once copied");

    let Request::Run {
        command,
        on_host: false,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no import");
    };
    assert!(command[2].contains("--clean --import"), "{command:?}");
    assert_eq!(command[4], password);
    host.complete(done, &ok("")).unwrap();

    let Request::Trust {
        name,
        path,
        done: Some(done),
        ..
    } = next(&mut host)
    else {
        panic!("no trust");
    };
    assert_eq!(
        (name.as_str(), path.as_path()),
        ("aspnetcore-dev", crt.as_path())
    );
    host.complete(done, &ok("/etc/ssl/certs/ca-certificates.crt"))
        .unwrap();
    assert_eq!(
        notes(&mut host),
        ["aspnet-devcerts: the devcontainer serves and trusts this machine's dev certificate"]
    );
    assert!(!crt.exists());

    // Containers without dotnet end the chain quietly.
    host.emit(&ready("local"));
    let Request::Run {
        done: Some(done), ..
    } = next(&mut host)
    else {
        panic!("no dotnet check");
    };
    host.complete(done, &JobResult::from_output(Some(1), b"", b""))
        .unwrap();
    assert!(drain(&mut host).is_empty());
}

#[test]
fn manifest_settings_reach_lua_with_the_users_values() {
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("copier");
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(
        ext.join("manifest.toml"),
        r#"description = "copies"
[[settings]]
key = "files"
title = "Files"
type = "list"
default = [".gitconfig"]

[[settings]]
key = "on_ssh"
type = "bool"

[[settings]]
key = "mode"
type = "choice"
choices = ["fast", "safe"]
"#,
    )
    .unwrap();
    std::fs::write(
        ext.join("init.lua"),
        r#"local s = term.settings()
           term.notify(table.concat(s.files, ",") .. " " .. tostring(s.on_ssh) .. " " .. s.mode)"#,
    )
    .unwrap();

    let mut host = ExtensionHost::new();
    host.load_dir(dir.path());
    assert_eq!(notes(&mut host), [".gitconfig false fast"]);
    let info = host.extensions().next().unwrap();
    assert_eq!(info.settings.len(), 3);
    assert_eq!(info.settings[0].title, "Files");

    use crate::ext::SettingValue;
    let mut own = std::collections::BTreeMap::new();
    own.insert(
        "files".to_owned(),
        SettingValue::List(vec![".tmux.conf".into(), ".inputrc".into()]),
    );
    own.insert("on_ssh".to_owned(), SettingValue::Bool(true));
    // Not one of the choices: the default stays.
    own.insert("mode".to_owned(), SettingValue::String("reckless".into()));
    let mut values = crate::ext::SettingValues::new();
    values.insert("copier".to_owned(), own);
    let mut host = ExtensionHost::new();
    host.load_dir_except(dir.path(), &[], &Default::default(), &values);
    assert_eq!(notes(&mut host), [".tmux.conf,.inputrc true fast"]);
}

#[test]
fn toolchains_opens_tool_homes_and_follows_their_variables() {
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("toolchains");
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(
        ext.join("manifest.toml"),
        include_str!("../../../../extensions/toolchains/manifest.toml"),
    )
    .unwrap();
    let stub = "local os_getenv = function(name) if name == 'RUSTUP_HOME' then return '/opt/rustup' end return nil end\n";
    let source = include_str!("../../../../extensions/toolchains/init.lua")
        .replace("os.getenv(", "os_getenv(");
    std::fs::write(ext.join("init.lua"), stub.to_owned() + &source).unwrap();

    use crate::ext::SettingValue;
    let mut own = std::collections::BTreeMap::new();
    own.insert("off".to_owned(), SettingValue::List(vec!["go".into()]));
    own.insert(
        "writable".to_owned(),
        SettingValue::List(vec!["~/.local/bin".into()]),
    );
    let mut values = crate::ext::SettingValues::new();
    values.insert("toolchains".to_owned(), own);
    let mut granted = std::collections::BTreeMap::new();
    granted.insert(
        "toolchains".to_owned(),
        vec!["env".to_owned(), "sandbox".to_owned()],
    );
    let mut host = ExtensionHost::new();
    let errors = host.load_dir_except(dir.path(), &[], &granted, &values);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(drain(&mut host).is_empty());

    let rules = host.sandbox_rules();
    for path in [
        "/opt/rustup",
        "~/.cargo/bin",
        "~/.nvm",
        "~/.npm",
        "~/.dotnet",
        "~/.local/bin",
    ] {
        assert!(
            rules.writable.iter().any(|p| p == path),
            "{path}: {:?}",
            rules.writable
        );
    }
    assert!(!rules.writable.iter().any(|p| p == "~/.rustup"));
    assert!(!rules.writable.iter().any(|p| p.starts_with("~/go")));
    for host_name in [
        "static.rust-lang.org",
        "nodejs.org",
        "registry.npmjs.org",
        "api.nuget.org",
    ] {
        assert!(rules.hosts.iter().any(|h| h == host_name), "{host_name}");
    }
    assert!(!rules.hosts.iter().any(|h| h == "proxy.golang.org"));
}

#[test]
fn the_old_global_name_still_works() {
    let mut host = ExtensionHost::new();
    host.load_source("old", r#"cmdterm.notify(tostring(cmdterm == term))"#)
        .unwrap();
    assert_eq!(notes(&mut host), ["true"]);
}

#[test]
fn the_type_files_declare_every_function_of_the_api() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "keys",
        r#"
        local names = {}
        for name in pairs(term) do names[#names + 1] = name end
        table.sort(names)
        term.notify(table.concat(names, " "))
        "#,
    )
    .unwrap();
    let names = notes(&mut host).remove(0);
    let types = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ext/types");
    let luals = std::fs::read_to_string(types.join("term.lua")).unwrap();
    let teal = std::fs::read_to_string(types.join("term.d.tl")).unwrap();
    for name in names.split(' ') {
        assert!(
            luals.contains(&format!("function term.{name}("))
                || luals.contains(&format!("---@field {name} ")),
            "types/term.lua lacks term.{name}"
        );
        assert!(
            teal.contains(&format!("\n   {name}: ")),
            "types/term.d.tl lacks {name}"
        );
        assert!(
            include_str!("README.md").contains(&format!("term.{name}")),
            "README.md lacks term.{name}"
        );
    }
}

#[test]
fn teal_project_files_are_not_extensions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tlconfig.lua"), "return {}").unwrap();
    std::fs::write(dir.path().join("real.lua"), "").unwrap();
    let mut host = ExtensionHost::new();
    assert!(host.load_dir(dir.path()).is_empty());
    let names: Vec<String> = host.extensions().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["real"]);
}

#[test]
fn tasks_register_and_run_for_a_project() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "cleanup",
        r#"
        term.task {
          name = "prune",
          title = "Prune worktrees",
          every = "1d",
          projects = { "cmdterm", "/src/*" },
          run = function(e)
            local paths = {}
            for _, w in ipairs(e.worktrees) do
              paths[#paths + 1] = w.path .. ":" .. (w.branch or "-") .. ":" .. w.runtime
            end
            term.notify(e.task .. " " .. e.project .. " " .. e.root .. " " .. e.env .. " "
              .. table.concat(paths, ","))
          end,
        }
        term.task { name = "global", every = 3600, run = function(e)
          term.notify(e.task .. " " .. tostring(e.project))
        end }
        "#,
    )
    .unwrap();
    let tasks = host.tasks();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].id, "cleanup.prune");
    assert_eq!(tasks[0].title, "Prune worktrees");
    assert_eq!(tasks[0].every, 24 * 60 * 60);
    assert_eq!(tasks[1].title, "global");
    assert_eq!(tasks[1].projects, None);
    let revision = host.tasks_revision();

    assert!(tasks[0].matches("cmdterm", &PathBuf::from("/elsewhere")));
    assert!(tasks[0].matches("other", &PathBuf::from("/src/a/b")));
    assert!(!tasks[0].matches("other", &PathBuf::from("/home/src")));
    assert!(!tasks[1].matches("cmdterm", &PathBuf::from("/src/a")));

    let project = crate::ext::TaskProject {
        name: "cmdterm".into(),
        id: "cmdterm-1".into(),
        root: "/src/cmdterm".into(),
        env: "local".into(),
        worktrees: vec![
            crate::ext::TaskWorktree {
                path: "/src/cmdterm".into(),
                branch: Some("main".into()),
                runtime: "host".into(),
            },
            crate::ext::TaskWorktree {
                path: "/src/cmdterm-x".into(),
                branch: None,
                runtime: "sandbox".into(),
            },
        ],
    };
    host.run_task("cleanup.prune", Some(&project)).unwrap();
    host.run_task("cleanup.global", None).unwrap();
    assert_eq!(
        notes(&mut host),
        [
            "cleanup.prune cmdterm /src/cmdterm local /src/cmdterm:main:host,/src/cmdterm-x:-:sandbox",
            "cleanup.global nil",
        ]
    );
    assert_eq!(
        host.run_task("cleanup.missing", None)
            .unwrap_err()
            .extension,
        "scriptschnell-term"
    );
    assert_eq!(host.tasks_revision(), revision);
}

#[test]
fn task_arguments_are_checked_and_a_failing_task_disables_its_extension() {
    for bad in [
        r#"term.task { name = "a b", every = 60, run = function() end }"#,
        r#"term.task { name = "a", every = 59, run = function() end }"#,
        r#"term.task { name = "a", every = "1y", run = function() end }"#,
        r#"term.task { name = "a", every = "1ä", run = function() end }"#,
        r#"term.task { name = "a", every = 60, projects = {}, run = function() end }"#,
        r#"term.task { name = "a", every = 60 }"#,
    ] {
        let mut host = ExtensionHost::new();
        assert!(host.load_source("bad", bad).is_err(), "{bad}");
    }
    let mut host = ExtensionHost::new();
    host.load_source(
        "failing",
        r#"term.task { name = "boom", every = "5m", run = function() error("no") end }"#,
    )
    .unwrap();
    assert_eq!(host.tasks()[0].every, 300);
    assert!(host.run_task("failing.boom", None).is_err());
    assert!(!enabled(&host, "failing"));
    assert!(host.tasks().is_empty());
}

#[test]
fn project_patterns_with_a_tilde_compare_with_the_root() {
    let home = std::env::var("HOME").unwrap();
    let task = crate::ext::TaskInfo {
        id: "x.y".into(),
        title: "y".into(),
        extension: "x".into(),
        every: 60,
        projects: Some(vec!["~/work/*".into(), "*-docs".into()]),
    };
    assert!(task.matches("a", &PathBuf::from(format!("{home}/work/a"))));
    assert!(!task.matches("a", &PathBuf::from("/work/a")));
    assert!(task.matches("site-docs", &PathBuf::from("/srv/site")));
    assert!(!task.matches("site-docs2", &PathBuf::from("/srv/site")));
}

#[test]
fn a_later_directory_replaces_extensions_of_the_same_name() {
    let shipped = tempfile::tempdir().unwrap();
    let own = tempfile::tempdir().unwrap();
    std::fs::write(shipped.path().join("npm.lua"), "term.notify('shipped npm')").unwrap();
    std::fs::write(
        shipped.path().join("cargo.lua"),
        "term.notify('shipped cargo')",
    )
    .unwrap();
    std::fs::create_dir(own.path().join("npm")).unwrap();
    std::fs::write(own.path().join("npm/init.lua"), "term.notify('own npm')").unwrap();
    std::fs::write(own.path().join("alpha.lua"), "term.notify('alpha')").unwrap();
    let mut host = ExtensionHost::new();
    let errors = host.load_dirs_except(
        &[shipped.path(), own.path(), own.path()],
        &[],
        &Default::default(),
        &Default::default(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(notes(&mut host), ["alpha", "shipped cargo", "own npm"]);
}

#[test]
fn the_store_keeps_json_values_across_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let config = || Config {
        data_dir: Some(dir.path().to_path_buf()),
        ..Config::default()
    };
    let mut host = ExtensionHost::with_config(config());
    host.load_source(
        "notes",
        r#"
        term.store.set("doc/a", { title = "A", tags = { "x", "y" }, n = 2, ok = true })
        term.store.set("doc/b", "plain")
        term.store.set("gone", 1)
        term.store.delete("gone")
        local a = term.store.get("doc/a")
        term.notify(a.title .. a.tags[2] .. a.n .. tostring(a.ok))
        term.notify(table.concat(term.store.keys("doc/"), ","))
        term.notify(tostring(term.store.get("gone")))
        term.notify(term.json_encode({ 1, 2 }) .. term.json_decode('{"k":[3]}').k[1])
        "#,
    )
    .unwrap();
    assert_eq!(
        notes(&mut host),
        ["Ay2true", "doc/a,doc/b", "nil", "[1,2]3"]
    );
    crate::ext::flush_stores();
    // A new host, as after a reload, reads what the first one wrote.
    let mut again = ExtensionHost::with_config(config());
    again
        .load_source("notes", "term.notify(term.store.get('doc/b'))")
        .unwrap();
    assert_eq!(notes(&mut again), ["plain"]);
    assert!(dir.path().join("notes").join("doc%2Fa.json").is_file());

    let mut bad = ExtensionHost::new();
    assert!(bad.load_source("bad", "term.store.set('a b', 1)").is_err());
    assert!(
        bad.load_source("worse", "term.store.set('k', print)")
            .is_err()
    );
}

#[test]
fn views_render_through_the_host() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "kb",
        r##"
        term.view{ name = "doc", render = function(key)
          if key == "missing" then return nil end
          return { title = "T " .. key, markdown = "# " .. key }
        end }
        term.open_view("doc", "c1/a")
        "##,
    )
    .unwrap();
    assert_eq!(
        drain(&mut host),
        [Request::OpenView {
            view: "kb.doc".into(),
            key: "c1/a".into()
        }]
    );
    let page = host.render_view("kb.doc", "c1/a").unwrap().unwrap();
    assert_eq!(
        (page.title.as_str(), page.markdown.as_str()),
        ("T c1/a", "# c1/a")
    );
    assert_eq!(host.render_view("kb.doc", "missing").unwrap(), None);
    assert_eq!(host.render_view("kb.nope", "x").unwrap(), None);
}

#[test]
fn tools_take_json_arguments_and_return_values_or_errors() {
    let mut host = ExtensionHost::new();
    host.load_source(
        "kb",
        r#"
        term.tool{
          name = "kb_echo",
          description = "Echoes",
          input = { properties = { text = { type = "string" } }, required = { "text" } },
          run = function(args, caller)
            if args.text == "" then return nil, "text is empty" end
            return { text = args.text, from = caller.environment, list = args.list }
          end,
        }
        term.tool{ name = "kb_crash", description = "Crashes", run = function() error("bad") end }
        "#,
    )
    .unwrap();
    let tool = &host.tools()[0];
    assert_eq!(tool.name, "kb_echo");
    assert_eq!(tool.input_schema["type"], "object");
    assert_eq!(tool.input_schema["required"][0], "text");
    let caller = crate::ext::ToolCaller {
        environment: "sandbox".into(),
        ..Default::default()
    };
    let answer = host
        .call_tool(
            "kb_echo",
            &serde_json::json!({ "text": "hi", "list": [1, 2] }),
            &caller,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        answer,
        serde_json::json!({ "text": "hi", "from": "sandbox", "list": [1, 2] })
    );
    assert_eq!(
        host.call_tool("kb_echo", &serde_json::json!({ "text": "" }), &caller)
            .unwrap(),
        Err("text is empty".to_owned())
    );
    assert!(
        host.call_tool("nope", &serde_json::json!({}), &caller)
            .is_err()
    );
    assert!(
        host.call_tool("kb_crash", &serde_json::json!({}), &caller)
            .is_err()
    );
    assert!(
        host.tools().is_empty(),
        "a failed extension's tools go away"
    );

    let without = host.load_source_with(
        "plain",
        "term.tool{ name = 'x', description = 'x', run = function() end }",
        Permissions::NONE,
    );
    assert!(without.unwrap_err().message.contains("'agents' permission"));
}

#[test]
fn knowledge_keeps_documents_for_agents() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let data = tempfile::tempdir().unwrap();
    let load = || {
        let mut host = ExtensionHost::with_config(Config {
            data_dir: Some(data.path().to_path_buf()),
            ..Config::default()
        });
        let source = std::fs::read_to_string(root.join("knowledge/init.lua")).unwrap();
        host.load_source("knowledge", &source).unwrap();
        host
    };
    let mut host = load();
    let caller = crate::ext::ToolCaller {
        environment: "sandbox".into(),
        agent: Some("app-1 (claude)".into()),
        ..Default::default()
    };
    let call = |host: &mut ExtensionHost, name: &str, args: serde_json::Value| {
        host.call_tool(name, &args, &caller).unwrap()
    };
    let created = call(
        &mut host,
        "knowledge_create",
        serde_json::json!({
            "collection": "Decisions", "title": "Use SQLite", "tags": ["Storage", "db"],
            "body": "We store **rows** in SQLite.\n\n```mermaid\ngraph TD\nA-->B\n```",
            "metadata": { "status": "accepted" },
        }),
    )
    .unwrap();
    assert_eq!(created["collection"], "decisions");
    assert_eq!(created["id"], "use-sqlite");
    assert_eq!(created["tags"], serde_json::json!(["storage", "db"]));
    let second = call(
        &mut host,
        "knowledge_create",
        serde_json::json!({ "collection": "decisions", "title": "Use SQLite", "body": "again" }),
    )
    .unwrap();
    assert_eq!(second["id"], "use-sqlite-2");
    assert!(
        call(
            &mut host,
            "knowledge_create",
            serde_json::json!({ "collection": "x" })
        )
        .unwrap_err()
        .contains("'title' is required")
    );

    let updated = call(
        &mut host,
        "knowledge_update",
        serde_json::json!({
            "collection": "decisions", "id": "use-sqlite", "append": "Postgres was too much.",
            "add_tags": ["adr"], "remove_tags": ["db"], "metadata": { "status": "", "owner": "me" },
        }),
    )
    .unwrap();
    assert_eq!(updated["tags"], serde_json::json!(["storage", "adr"]));
    assert_eq!(updated["metadata"], serde_json::json!({ "owner": "me" }));

    let found = call(
        &mut host,
        "knowledge_search",
        serde_json::json!({ "query": "postgres sqlite" }),
    )
    .unwrap();
    assert_eq!(found["results"][0]["id"], "use-sqlite");
    assert!(
        found["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("Postgres")
    );
    let listed = call(
        &mut host,
        "knowledge_list",
        serde_json::json!({ "tags": ["adr"] }),
    )
    .unwrap();
    assert_eq!(listed["total"], 1);

    call(
        &mut host,
        "knowledge_open",
        serde_json::json!({ "collection": "decisions", "id": "use-sqlite" }),
    )
    .unwrap();
    let page = host
        .render_view("knowledge.doc", "app|decisions/use-sqlite")
        .unwrap()
        .unwrap();
    assert_eq!(page.title, "Use SQLite");
    assert!(page.markdown.contains("| owner | me |"));
    assert!(page.markdown.contains("```mermaid"));
    assert!(page.markdown.ends_with("Postgres was too much."));
    let overview = host
        .render_view("knowledge.collection", "app|")
        .unwrap()
        .unwrap();
    assert!(overview.markdown.contains("`use-sqlite-2`"));

    call(
        &mut host,
        "knowledge_delete",
        serde_json::json!({ "collection": "decisions", "id": "use-sqlite-2" }),
    )
    .unwrap();
    assert!(
        drain(&mut host)
            .iter()
            .any(|r| matches!(r, Request::OpenView { view, .. } if view == "knowledge.doc"))
    );

    // A reload reads the documents back from the store.
    crate::ext::flush_stores();
    let mut again = load();
    let viewed = call(
        &mut again,
        "knowledge_view",
        serde_json::json!({ "collection": "decisions", "id": "use-sqlite" }),
    )
    .unwrap();
    assert_eq!(viewed["author"], "app-1 (claude)");
    assert_eq!(
        call(&mut again, "knowledge_collections", serde_json::json!({})).unwrap()["collections"][0]
            ["documents"],
        1
    );
}

#[test]
fn project_stores_keep_each_projects_data_apart_and_need_their_permission() {
    let dir = tempfile::tempdir().unwrap();
    let config = || Config {
        data_dir: Some(dir.path().join("app")),
        project_data_dir: Some(dir.path().join("projects")),
        ..Config::default()
    };
    let mut host = ExtensionHost::with_config(config());
    host.load_source_with(
        "notes",
        r#"
        term.command{ name = "write", run = function()
          local here = term.project_store()
          here.set("last", term.context().project)
          term.project_store("other-2").set("last", "elsewhere")
          term.notify(here.id .. " " .. here.get("last") .. " "
            .. term.project_store("other-2").get("last") .. " "
            .. table.concat(here.keys(), ","))
        end }
        "#,
        [Permission::ProjectStorage].into_iter().collect(),
    )
    .unwrap();
    assert!(
        host.run_command("write").is_err(),
        "no project is active yet"
    );
    host.load_source_with(
        "notes",
        r#"
        term.command{ name = "write", run = function()
          local here = term.project_store()
          here.set("last", term.context().project)
          term.project_store("other-2").set("last", "elsewhere")
          term.notify(here.id .. " " .. here.get("last") .. " "
            .. term.project_store("other-2").get("last") .. " "
            .. table.concat(here.keys(), ","))
        end }
        "#,
        [Permission::ProjectStorage].into_iter().collect(),
    )
    .unwrap();
    host.set_context(Context {
        project: Some("app".into()),
        project_id: Some("app-1".into()),
        ..Context::default()
    });
    host.run_command("write").unwrap();
    assert_eq!(notes(&mut host), ["app-1 app elsewhere last"]);
    crate::ext::flush_stores();
    assert!(dir.path().join("projects/app-1/notes/last.json").is_file());
    assert!(
        dir.path()
            .join("projects/other-2/notes/last.json")
            .is_file()
    );

    // The app-wide store needs `storage`, the project stores `project_storage`.
    let mut plain = ExtensionHost::with_config(config());
    let error = plain
        .load_source_with(
            "a",
            "term.store.set('k', 1)",
            [Permission::ProjectStorage].into_iter().collect(),
        )
        .unwrap_err();
    assert!(error.message.contains("'storage' permission"), "{error}");
    let error = plain
        .load_source_with(
            "b",
            "term.project_store('p')",
            [Permission::Storage].into_iter().collect(),
        )
        .unwrap_err();
    assert!(
        error.message.contains("'project_storage' permission"),
        "{error}"
    );
    let error = plain
        .load_source_with(
            "c",
            "term.project_store('../x')",
            [Permission::ProjectStorage].into_iter().collect(),
        )
        .unwrap_err();
    assert!(error.message.contains("not a project_id"), "{error}");
}

#[test]
fn knowledge_keeps_documents_per_project_when_its_scope_says_so() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let data = tempfile::tempdir().unwrap();
    let mut host = ExtensionHost::with_config(Config {
        data_dir: Some(data.path().join("app")),
        project_data_dir: Some(data.path().join("projects")),
        ..Config::default()
    });
    let off: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name != "knowledge")
        .collect();
    let grants = std::collections::BTreeMap::from([(
        "knowledge".to_owned(),
        ["agents", "storage", "project_storage"]
            .map(str::to_owned)
            .to_vec(),
    )]);
    let values = crate::ext::SettingValues::from([(
        "knowledge".to_owned(),
        std::collections::BTreeMap::from([(
            "scope".to_owned(),
            crate::ext::SettingValue::String("project".into()),
        )]),
    )]);
    let errors = host.load_dirs_except(&[&root], &off, &grants, &values);
    assert!(errors.is_empty(), "{errors:?}");

    let caller = |project: &str| crate::ext::ToolCaller {
        project_id: Some(project.into()),
        ..Default::default()
    };
    let create = serde_json::json!({ "collection": "notes", "title": "Only here", "body": "x" });
    host.call_tool("knowledge_create", &create, &caller("app-1"))
        .unwrap()
        .unwrap();
    let mine = host
        .call_tool("knowledge_list", &serde_json::json!({}), &caller("app-1"))
        .unwrap()
        .unwrap();
    let theirs = host
        .call_tool("knowledge_list", &serde_json::json!({}), &caller("lib-2"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (mine["total"].as_i64(), theirs["total"].as_i64()),
        (Some(1), Some(0))
    );
    assert!(
        host.call_tool(
            "knowledge_list",
            &serde_json::json!({}),
            &crate::ext::ToolCaller::default()
        )
        .unwrap()
        .unwrap_err()
        .contains("per project")
    );

    let page = host
        .render_view("knowledge.doc", "app-1|notes/only-here")
        .unwrap()
        .unwrap();
    assert_eq!(page.title, "Only here");
    assert_eq!(
        host.render_view("knowledge.doc", "app|notes/only-here")
            .unwrap(),
        None
    );
    crate::ext::flush_stores();
    assert!(data.path().join("projects/app-1/knowledge").is_dir());
    assert!(!data.path().join("app/knowledge").exists());
}

#[test]
fn require_cannot_leave_the_extension_directory_without_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.lua"), "return 'LEAKED'").unwrap();
    let ext = dir.path().join("ext");
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(ext.join("helper.lua"), "return 'sibling'").unwrap();
    // Without `filesystem`, `require` finds the sibling module next to
    // `init.lua` but must not follow a widened `package.path` to the rest of
    // the disk, even though the file is there and readable.
    std::fs::write(
        ext.join("init.lua"),
        format!(
            r#"
            term.notify("sib:" .. require("helper"))
            package.path = {:?} .. "/?.lua"
            local ok, error = pcall(require, "secret")
            term.notify("escape:" .. tostring(ok))
            "#,
            outside.display().to_string()
        ),
    )
    .unwrap();

    let mut host = ExtensionHost::new();
    let errors = host.load_dir(dir.path());
    assert!(errors.is_empty(), "{errors:?}");
    let messages = notes(&mut host);
    assert_eq!(messages, ["sib:sibling", "escape:false"], "{messages:?}");
    assert!(
        !messages.iter().any(|m| m.contains("LEAKED")),
        "{messages:?}"
    );
}

#[test]
fn require_stays_in_the_directory_through_parent_paths_without_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.lua"), "return 'LEAKED'").unwrap();
    let ext = dir.path().join("ext");
    std::fs::create_dir(&ext).unwrap();
    // A `..` segment resolves out of the directory. A plain prefix check on
    // the path would call `<ext>/../outside/secret.lua` in-directory;
    // resolving it first must show it is not.
    std::fs::write(
        ext.join("init.lua"),
        format!(
            r#"
            package.path = {:?} .. "/../outside/?.lua"
            local ok = pcall(require, "secret")
            term.notify("escape:" .. tostring(ok))
            "#,
            ext.display().to_string()
        ),
    )
    .unwrap();

    let mut host = ExtensionHost::new();
    let errors = host.load_dir(dir.path());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(notes(&mut host), ["escape:false"]);
}

#[test]
fn require_reads_the_wider_disk_with_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.lua"), "return 'LEAKED'").unwrap();
    // With `filesystem` the extension may read beyond its own directory, so
    // the hook that keeps a lesser extension in its directory steps aside.
    let source = format!(
        r#"package.path = {:?} .. "/?.lua" term.notify("got:" .. require("secret"))"#,
        outside.display().to_string()
    );
    let mut host = ExtensionHost::new();
    host.load_source_with(
        "reader",
        &source,
        [Permission::Filesystem].into_iter().collect(),
    )
    .unwrap();
    assert_eq!(notes(&mut host), ["got:LEAKED"]);
}

#[test]
fn forges_register_and_need_the_network_permission() {
    let source = r#"
        term.forge{
          name = "hub", title = "Hub", accounts = { default = { "Hub.example" } },
          aliases = { ["*.hub.example"] = "hub.example" },
          auth = { header = "Authorization", value = "token {token}" },
          env = { "HUB_TOKEN" },
          token_url = function(host) return "https://" .. host .. "/tokens" end,
          whoami = function(host) return "https://" .. host .. "/user", "login" end,
          check = function(items)
            for _, item in ipairs(items) do
              term.http{ url = "https://" .. item.account .. "/pulls/" .. item.branches[1],
                         account = item.account,
                         done = function(res)
                           term.forge_report("hub", {
                             { account = item.account, path = item.path, branch = item.branches[1],
                               state = res.ok and "merged" or "none", number = 7, url = res.body },
                           }, { retry_after = res.headers["retry-after"] and 60 })
                         end }
            end
          end,
        }
    "#;
    let mut host = ExtensionHost::new();
    let error = host
        .load_source_with("hub", source, Permissions::from_names(&["storage"]))
        .unwrap_err();
    assert!(error.message.contains("'network' permission"), "{error}");

    let mut host = ExtensionHost::new();
    host.load_source_with("hub", source, Permissions::from_names(&["network"]))
        .unwrap();
    let forge = host.forges()[0].clone();
    assert_eq!(forge.id, "hub.hub");
    assert_eq!(forge.hosts, ["hub.example"]);
    assert_eq!(
        forge.account_for("git.hub.example", "o/r"),
        Some("hub.example")
    );
    assert_eq!(
        host.forge_token_url("hub.hub", "hub.example")
            .unwrap()
            .as_deref(),
        Some("https://hub.example/tokens")
    );
    assert_eq!(
        host.forge_whoami("hub.hub", "hub.example").unwrap(),
        Some(("https://hub.example/user".into(), Some("login".into())))
    );

    let check = ForgeCheck {
        account: "hub.example".into(),
        host: "hub.example".into(),
        path: "o/r".into(),
        remote: "git@hub.example:o/r.git".into(),
        branches: vec!["fix".into()],
        signed_in: true,
    };
    host.forge_check("hub.hub", &[check]).unwrap();
    let requests = drain(&mut host);
    let [
        Request::Http(HttpRequest {
            url,
            account,
            done: Some(done),
            ..
        }),
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    assert_eq!(url, "https://hub.example/pulls/fix");
    assert_eq!(account, &Some(("hub.hub".into(), "hub.example".into())));
    let response = HttpResponse {
        status: 200,
        headers: vec![("retry-after".into(), "5".into())],
        body: "https://hub.example/o/r/pulls/7".into(),
        error: None,
    };
    host.complete_http(*done, &response).unwrap();
    assert_eq!(
        drain(&mut host),
        [Request::ForgeReport {
            forge: "hub.hub".into(),
            results: vec![BranchReport {
                account: "hub.example".into(),
                path: "o/r".into(),
                branch: "fix".into(),
                state: BranchState::Merged,
                number: Some(7),
                url: Some("https://hub.example/o/r/pulls/7".into()),
                title: None,
            }],
            retry_after: Some(60),
        }]
    );
}

#[test]
fn forge_registrations_check_their_fields() {
    let mut host = ExtensionHost::new();
    for bad in [
        r#"term.forge{ name = "x", accounts = { default = { "*.wild" } }, check = function() end }"#,
        r#"term.forge{ name = "x", accounts = { default = { "a.com/../b" } }, check = function() end }"#,
        r#"term.forge{ name = "x", accounts = { default = { "a.com//b" } }, check = function() end }"#,
        r#"term.forge{ name = "x", aliases = { ["b.com"] = "a.com/*" }, check = function() end }"#,
        r#"term.forge{ name = "x", aliases = { ["*.b.com"] = "*.a.com" }, check = function() end }"#,
        r#"term.forge{ name = "x", auth = { value = "Bearer" }, check = function() end }"#,
        r#"term.forge{ name = "x" }"#,
        r#"term.forge{ name = "x", hosts = { "a.com" }, check = function() end }"#,
        r#"term.forge{ name = "x", setting = "hosts", check = function() end }"#,
        r#"term.forge{ name = "x", accounts = "a.com", check = function() end }"#,
        r#"term.http{ url = "file:///etc/passwd" }"#,
        r#"term.http{ url = "https://a", method = "DELETE" }"#,
        r#"term.http{ url = "https://a", account = "a" }"#,
        r#"term.forge_report("nobody", {})"#,
    ] {
        assert!(
            host.load_source_with("x", bad, Permissions::from_names(&["network"]))
                .is_err(),
            "{bad}"
        );
    }
}

#[test]
fn forge_accounts_may_name_a_path() {
    let source = r#"
        term.forge{
          name = "hub", accounts = { default = { "Hub.example/Acme/", "hub.example", "hub.example/acme" } },
          aliases = { ["*.hub.example/v3"] = "hub.example/*" },
          token_url = function(host, account) return "https://" .. host .. "/tokens#" .. account end,
          check = function() end,
        }
    "#;
    let mut host = ExtensionHost::new();
    host.load_source_with("hub", source, Permissions::from_names(&["network"]))
        .unwrap();
    let forge = host.forges()[0].clone();
    assert_eq!(forge.hosts, ["hub.example/acme", "hub.example"]);
    assert_eq!(
        forge.account_for("acme.hub.example", "v3/r"),
        Some("hub.example/acme")
    );
    assert_eq!(
        host.forge_token_url("hub.hub", "hub.example/acme")
            .unwrap()
            .as_deref(),
        Some("https://hub.example/tokens#hub.example/acme")
    );
}

#[test]
fn shipped_forges_send_requests_of_path_accounts_to_their_host() {
    let cases = [
        (
            "github",
            "github.com/acme",
            "https://api.github.com/graphql",
        ),
        (
            "gitlab",
            "gitlab.com/acme",
            "https://gitlab.com/api/graphql",
        ),
        (
            "forgejo",
            "codeberg.org/acme",
            "https://codeberg.org/api/v1/repos/acme/r/pulls?",
        ),
        (
            "gitea",
            "gitea.com/acme",
            "https://gitea.com/api/v1/repos/acme/r/pulls?",
        ),
        (
            "bitbucket",
            "bitbucket.org/acme",
            "https://api.bitbucket.org/2.0/repositories/acme/r/pullrequests?",
        ),
    ];
    for (name, account, url) in cases {
        let mut host = forge_host(name);
        let id = host.forges()[0].id.clone();
        let item = check(
            account,
            crate::ext::account_host(account),
            "acme/r",
            &["fix"],
        );
        host.forge_check(&id, &[item]).unwrap();
        let requests = drain(&mut host);
        let [Request::Http(request)] = requests.as_slice() else {
            panic!("{name}: {requests:?}");
        };
        assert!(request.url.starts_with(url), "{name}: {}", request.url);
        assert_eq!(request.account, Some((id, account.to_owned())), "{name}");
    }
}

#[test]
fn azure_devops_makes_one_account_per_organization() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join("azure-devops");
    std::fs::create_dir(&ext).unwrap();
    for file in ["init.lua", "manifest.toml"] {
        std::fs::copy(root.join("azure-devops").join(file), ext.join(file)).unwrap();
    }
    let mut host = ExtensionHost::new();
    let accounts = [(
        "azure-devops.azure-devops".to_owned(),
        vec![
            "dev.azure.com/contoso".into(),
            "dev.azure.com/fabrikam".into(),
        ],
    )];
    host.set_forge_hosts(&accounts.into());
    let granted = [("azure-devops".to_owned(), vec!["network".to_owned()])].into();
    let errors = host.load_dir_except(dir.path(), &[], &granted, &crate::ext::SettingValues::new());
    assert!(errors.is_empty(), "{errors:?}");
    let forge = host.forges()[0].clone();
    assert_eq!(
        forge.hosts,
        ["dev.azure.com/contoso", "dev.azure.com/fabrikam"]
    );
    assert_eq!(
        forge.account_for("ssh.dev.azure.com", "v3/fabrikam/p/r"),
        Some("dev.azure.com/fabrikam")
    );
    assert_eq!(
        forge.account_for("contoso.visualstudio.com", "DefaultCollection/p/_git/r"),
        Some("dev.azure.com/contoso")
    );
    assert_eq!(
        host.forge_token_url("azure-devops.azure-devops", "dev.azure.com/contoso")
            .unwrap()
            .as_deref(),
        Some("https://dev.azure.com/contoso/_usersSettings/tokens")
    );
}

/// Loads one shipped forge extension with its settings' defaults.
fn forge_host(name: &str) -> ExtensionHost {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(name)).unwrap();
    for file in ["init.lua", "manifest.toml"] {
        std::fs::copy(root.join(name).join(file), dir.path().join(name).join(file)).unwrap();
    }
    let mut host = ExtensionHost::new();
    let errors = host.load_dir(dir.path());
    assert!(errors.is_empty(), "{errors:?}");
    host
}

/// Runs `name`'s check of `check`, answers its one request with `body`, and
/// returns the request and what the forge reported.
fn forge_round(name: &str, check: ForgeCheck, body: &str) -> (HttpRequest, Vec<BranchReport>) {
    let mut host = forge_host(name);
    let id = host.forges()[0].id.clone();
    host.forge_check(&id, &[check]).unwrap();
    let requests = drain(&mut host);
    let [Request::Http(request)] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    let response = HttpResponse {
        status: 200,
        body: body.into(),
        ..HttpResponse::default()
    };
    host.complete_http(request.done.unwrap(), &response)
        .unwrap();
    let reports = drain(&mut host);
    let [Request::ForgeReport { results, .. }] = reports.as_slice() else {
        panic!("{reports:?}");
    };
    (request.clone(), results.clone())
}

fn check(account: &str, host: &str, path: &str, branches: &[&str]) -> ForgeCheck {
    ForgeCheck {
        account: account.into(),
        host: host.into(),
        path: path.into(),
        remote: String::new(),
        branches: branches.iter().map(|b| (*b).to_owned()).collect(),
        signed_in: true,
    }
}

#[test]
fn github_asks_for_every_branch_in_one_graphql_query() {
    let body = r#"{"data":{"r1":{"b1":{"nodes":[{"number":4,"state":"MERGED","url":"https://github.com/o/r/pull/4","title":"Fix"}]},
        "b2":{"nodes":[]}},"rateLimit":{"remaining":4999}}}"#;
    let (request, results) = forge_round(
        "github",
        check("github.com", "github.com", "o/r", &["fix", "new"]),
        body,
    );
    assert_eq!(request.url, "https://api.github.com/graphql");
    assert_eq!(request.method, "POST");
    let query = request.body.unwrap();
    assert!(query.contains(r#"headRefName: \"fix\""#), "{query}");
    assert!(query.contains(r#"headRefName: \"new\""#), "{query}");
    assert_eq!(results[0].state, BranchState::Merged);
    assert_eq!(results[0].number, Some(4));
    assert_eq!(results[1].state, BranchState::None);
}

#[test]
fn gitlab_prefers_an_open_merge_request() {
    let body = r#"{"data":{"p1":{"mergeRequests":{"nodes":[
        {"iid":"9","state":"closed","webUrl":"u9","title":"Old","sourceBranch":"fix"},
        {"iid":"8","state":"opened","webUrl":"u8","title":"New","sourceBranch":"fix"}]}}}}"#;
    let (request, results) = forge_round(
        "gitlab",
        check("gitlab.com", "gitlab.com", "g/sub/r", &["fix"]),
        body,
    );
    assert_eq!(request.url, "https://gitlab.com/api/graphql");
    assert!(request.body.unwrap().contains("sourceBranches"));
    assert_eq!(results[0].state, BranchState::Open);
    assert_eq!(results[0].number, Some(8));
}

#[test]
fn forgejo_matches_head_branches_of_one_list() {
    let body = r#"[{"number":3,"title":"T","html_url":"https://codeberg.org/o/r/pulls/3","state":"closed","merged":true,"head":{"ref":"fix"}}]"#;
    let (request, results) = forge_round(
        "forgejo",
        check("codeberg.org", "codeberg.org", "o/r", &["fix", "x"]),
        body,
    );
    assert_eq!(
        request.url,
        "https://codeberg.org/api/v1/repos/o/r/pulls?state=all&sort=recentupdate&limit=50"
    );
    assert_eq!(results[0].state, BranchState::Merged);
    assert_eq!(results[1].state, BranchState::None);
}

#[test]
fn azure_devops_lists_a_project_once_for_ssh_and_https_remotes() {
    let body = r#"{"value":[{"pullRequestId":5,"status":"completed","title":"T",
        "sourceRefName":"refs/heads/fix","repository":{"name":"Repo"}}]}"#;
    let (request, results) = forge_round(
        "azure-devops",
        check(
            "dev.azure.com",
            "ssh.dev.azure.com",
            "v3/org/My Proj/Repo",
            &["fix"],
        ),
        body,
    );
    assert!(
        request
            .url
            .starts_with("https://dev.azure.com/org/My%20Proj/_apis/git/pullrequests?"),
        "{}",
        request.url
    );
    assert_eq!(results[0].state, BranchState::Merged);
    assert_eq!(
        results[0].url.as_deref(),
        Some("https://dev.azure.com/org/My%20Proj/_git/Repo/pullrequest/5")
    );
}

#[test]
fn bitbucket_filters_branches_in_one_request_per_repository() {
    let body = r#"{"values":[
        {"id":12,"title":"T","state":"DECLINED","links":{"html":{"href":"https://bitbucket.org/ws/app/pull-requests/12"}},"source":{"branch":{"name":"fix"}}},
        {"id":13,"title":"T2","state":"MERGED","links":{"html":{"href":"https://bitbucket.org/ws/app/pull-requests/13"}},"source":{"branch":{"name":"fix"}}}]}"#;
    let (request, results) = forge_round(
        "bitbucket",
        check(
            "bitbucket.org",
            "bitbucket.org",
            "ws/app",
            &["fix", "other"],
        ),
        body,
    );
    assert!(
        request
            .url
            .starts_with("https://api.bitbucket.org/2.0/repositories/ws/app/pullrequests?"),
        "{}",
        request.url
    );
    assert!(
        request.url.contains(
            "q=source.branch.name%20%3D%20%22fix%22%20OR%20source.branch.name%20%3D%20%22other%22"
        ),
        "{}",
        request.url
    );
    assert_eq!(results[0].state, BranchState::Merged);
    assert_eq!(results[0].number, Some(13));
    assert_eq!(
        results[0].url.as_deref(),
        Some("https://bitbucket.org/ws/app/pull-requests/13")
    );
    assert_eq!(results[1].state, BranchState::None);
}

/// Asks forge extension `name` for a discussion and answers each of its
/// requests with the body of the first `answers` entry whose key the URL
/// contains, until it reports the discussion.
fn discussion_round(
    name: &str,
    account: &str,
    path: &str,
    answers: &[(&str, &str)],
) -> (Vec<String>, crate::ext::Discussion) {
    let mut host = forge_host(name);
    let id = host.forges()[0].id.clone();
    assert!(host.forges()[0].discussions);
    let request = crate::ext::DiscussionRequest {
        id: "7".into(),
        account: account.into(),
        host: account.into(),
        path: path.into(),
        remote: String::new(),
        branch: "fix".into(),
        number: 12,
        signed_in: true,
    };
    assert!(host.forge_discussion(&id, &request).unwrap());
    let mut urls = Vec::new();
    for _ in 0..4 {
        for request in drain(&mut host) {
            match request {
                Request::Http(http) => {
                    let body = answers
                        .iter()
                        .find(|(key, _)| http.url.contains(key))
                        .map_or("null", |(_, body)| body);
                    urls.push(http.url.clone());
                    let response = HttpResponse {
                        status: 200,
                        body: body.into(),
                        ..HttpResponse::default()
                    };
                    host.complete_http(http.done.unwrap(), &response).unwrap();
                }
                Request::ForgeDiscussion { id, discussion, .. } => {
                    assert_eq!(id, "7");
                    return (urls, discussion.unwrap());
                }
                other => panic!("{other:?}"),
            }
        }
    }
    panic!("no discussion after {urls:?}");
}

#[test]
fn github_reads_threads_and_force_pushes_in_one_query() {
    let body = r#"{"data":{"repository":{"pullRequest":{"title":"Fix","url":"u","state":"OPEN",
        "body":"Why","author":{"login":"ann"},"isDraft":true,"baseRefName":"main","mergeable":"CONFLICTING",
        "latestReviews":{"nodes":[{"author":{"login":"cy"},"state":"APPROVED"}]},
        "reviewRequests":{"nodes":[{"requestedReviewer":{"name":"core"}}]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
          {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://ci/1"},
          {"__typename":"CheckRun","name":"test","status":"IN_PROGRESS","conclusion":null},
          {"__typename":"StatusContext","context":"deploy","state":"SUCCESS","targetUrl":"https://ci/2"}]}}}}]},
        "comments":{"nodes":[{"author":{"login":"bob"},"body":"Looks good","createdAt":"2026-01-02T00:00:00Z"}]},
        "reviews":{"nodes":[{"author":{"login":"cy"},"body":"","state":"APPROVED","submittedAt":"2026-01-03T00:00:00Z"}]},
        "reviewThreads":{"nodes":[{"path":"src/a.rs","originalLine":7,"diffSide":"RIGHT","isResolved":false,"isOutdated":true,
          "comments":{"nodes":[{"author":{"login":"bob"},"body":"Rename","createdAt":"2026-01-01T00:00:00Z",
            "diffHunk":"@@ -1 +1 @@\n+let x = 1;","originalCommit":{"oid":"abc123"}}]}}]},
        "timelineItems":{"nodes":[{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-01-04T00:00:00Z",
          "beforeCommit":{"oid":"abc123"},"afterCommit":{"oid":"def456"}}]}}}}}"#;
    let (urls, d) = discussion_round("github", "github.com", "o/r", &[("graphql", body)]);
    assert_eq!(urls, ["https://api.github.com/graphql"]);
    assert_eq!(d.state, BranchState::Open);
    assert_eq!(d.comments.len(), 2);
    assert!(d.comments[1].body.starts_with("Approved"));
    let thread = &d.threads[0];
    assert_eq!(
        (thread.line, thread.old_side, thread.outdated),
        (Some(7), false, true)
    );
    assert_eq!(thread.commit.as_deref(), Some("abc123"));
    assert!(thread.hunk.as_deref().unwrap().ends_with("let x = 1;"));
    assert!(d.pushes[0].force);
    assert_eq!(d.pushes[0].before.as_deref(), Some("abc123"));
    assert!(d.draft && d.conflicts);
    assert_eq!(d.target.as_deref(), Some("main"));
    let reviews: Vec<_> = d
        .reviews
        .iter()
        .map(|r| (r.author.as_str(), r.verdict))
        .collect();
    assert_eq!(
        reviews,
        [
            ("cy", crate::ext::Verdict::Approved),
            ("core", crate::ext::Verdict::Pending)
        ]
    );
    let checks: Vec<_> = d
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.state, c.url.as_deref()))
        .collect();
    assert_eq!(
        checks,
        [
            (
                "build",
                crate::ext::CheckState::Failed,
                Some("https://ci/1")
            ),
            ("test", crate::ext::CheckState::Running, None),
            (
                "deploy",
                crate::ext::CheckState::Success,
                Some("https://ci/2")
            ),
        ]
    );
}

#[test]
fn gitlab_reads_discussions_and_versions() {
    let mr = r#"{"title":"Fix","web_url":"u","state":"merged","author":{"username":"ann"},"description":"d",
        "draft":true,"target_branch":"main","reviewers":[{"username":"cy"},{"username":"dan"}],
        "head_pipeline":{"id":99,"status":"failed","web_url":"p"}}"#;
    let approvals = r#"{"approved_by":[{"user":{"username":"cy"}}]}"#;
    let jobs = r#"[{"name":"unit","stage":"test","status":"failed","web_url":"j"}]"#;
    let discussions = r#"[
        {"notes":[{"system":true,"body":"added 1 commit","author":{"username":"ann"}}]},
        {"notes":[{"body":"Hi","author":{"username":"bob"},"created_at":"t"}]},
        {"notes":[{"body":"Here","author":{"username":"bob"},"resolved":true,
          "position":{"new_path":"a.rs","new_line":4,"old_line":null,"head_sha":"h1","base_sha":"b1"}},
          {"body":"Done","author":{"username":"ann"}}]}]"#;
    let versions = r#"[{"head_commit_sha":"h2","created_at":"t2"},{"head_commit_sha":"h1","created_at":"t1"}]"#;
    let (urls, d) = discussion_round(
        "gitlab",
        "gitlab.com",
        "g/sub/r",
        &[
            ("/discussions", discussions),
            ("/versions", versions),
            ("/approvals", approvals),
            ("/pipelines/99/jobs", jobs),
            ("merge_requests/12", mr),
        ],
    );
    assert!(
        urls.iter()
            .all(|u| { u.starts_with("https://gitlab.com/api/v4/projects/g%2Fsub%2Fr/") })
    );
    assert!(d.draft);
    assert_eq!(d.target.as_deref(), Some("main"));
    let reviews: Vec<_> = d
        .reviews
        .iter()
        .map(|r| (r.author.as_str(), r.verdict))
        .collect();
    assert_eq!(
        reviews,
        [
            ("cy", crate::ext::Verdict::Approved),
            ("dan", crate::ext::Verdict::Pending)
        ]
    );
    let checks: Vec<_> = d
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.state))
        .collect();
    assert_eq!(
        checks,
        [
            ("pipeline #99", crate::ext::CheckState::Failed),
            ("test: unit", crate::ext::CheckState::Failed),
        ]
    );
    assert_eq!(d.state, BranchState::Merged);
    assert_eq!(d.comments.len(), 1);
    let thread = &d.threads[0];
    assert_eq!(
        (thread.path.as_str(), thread.line, thread.resolved),
        ("a.rs", Some(4), true)
    );
    assert_eq!(thread.commit.as_deref(), Some("h1"));
    assert_eq!(thread.comments.len(), 2);
    assert_eq!(d.pushes.len(), 2);
}

#[test]
fn gitea_groups_review_comments_and_reads_force_pushes_from_the_timeline() {
    for name in ["gitea", "forgejo"] {
        let account = if name == "gitea" {
            "gitea.com"
        } else {
            "codeberg.org"
        };
        let pull = r#"{"title":"Fix","html_url":"u","state":"closed","merged":true,"user":{"login":"ann"}}"#;
        let reviews = r#"[{"id":5,"user":{"login":"bob"},"state":"REQUEST_CHANGES","body":"","comments_count":2}]"#;
        let comments = r#"[{"path":"a.go","position":3,"original_position":0,"commit_id":"c1","diff_hunk":"@@\n+x","body":"One","user":{"login":"bob"},"created_at":"1"},
            {"path":"a.go","position":3,"original_position":0,"commit_id":"c1","body":"Two","user":{"login":"ann"},"created_at":"2"}]"#;
        let timeline = r#"[{"type":"pull_push","created_at":"t","body":"{\"is_force_push\":true,\"commit_ids\":[\"c1\",\"c2\"]}"}]"#;
        let (_, d) = discussion_round(
            name,
            account,
            "o/r",
            &[
                ("/reviews/5/comments", comments),
                ("/reviews", reviews),
                ("/timeline", timeline),
                ("/issues/12/comments", "[]"),
                ("/pulls/12", pull),
            ],
        );
        assert_eq!(d.state, BranchState::Merged, "{name}");
        assert!(d.comments[0].body.starts_with("Requested changes"));
        assert_eq!(d.threads.len(), 1, "{name}");
        assert_eq!(d.threads[0].comments.len(), 2);
        assert_eq!(d.threads[0].line, Some(3));
        assert!(d.pushes[0].force);
        assert_eq!(d.pushes[0].commit, "c2");
    }
}

#[test]
fn azure_devops_reads_threads_and_iterations() {
    let pr = r#"{"title":"Fix","status":"active","createdBy":{"displayName":"Ann"},"repository":{"name":"r"}}"#;
    let threads = r#"{"value":[
        {"status":"active","comments":[{"commentType":"system","content":"voted"}]},
        {"status":"fixed","threadContext":{"filePath":"/src/a.cs","rightFileStart":{"line":9}},
         "pullRequestThreadContext":{"iterationContext":{"secondComparingIteration":2}},
         "comments":[{"author":{"displayName":"Bob"},"content":"Here","publishedDate":"t"}]},
        {"status":"active","comments":[{"author":{"displayName":"Bob"},"content":"Overall","publishedDate":"t"}]}]}"#;
    let iterations = r#"{"value":[{"id":1,"sourceRefCommit":{"commitId":"i1"},"reason":"create"},
        {"id":2,"sourceRefCommit":{"commitId":"i2"},"reason":"forcePush"}]}"#;
    let (urls, d) = discussion_round(
        "azure-devops",
        "dev.azure.com",
        "org/proj/_git/r",
        &[
            ("/threads", threads),
            ("/iterations", iterations),
            ("pullRequests/12", pr),
        ],
    );
    assert!(
        urls.iter().all(|u| u.contains("api-version=7.1")),
        "{urls:?}"
    );
    assert_eq!(d.state, BranchState::Open);
    assert_eq!(d.comments.len(), 1);
    let thread = &d.threads[0];
    assert_eq!(
        (thread.path.as_str(), thread.line, thread.resolved),
        ("src/a.cs", Some(9), true)
    );
    assert_eq!(thread.commit.as_deref(), Some("i2"));
    assert!(d.pushes[1].force && !d.pushes[0].force);
}

#[test]
fn bitbucket_threads_replies_under_their_inline_root() {
    let pr = r#"{"title":"Fix","state":"OPEN","author":{"display_name":"Ann"},"links":{"html":{"href":"u"}}}"#;
    let comments = r#"{"values":[
        {"id":1,"content":{"raw":"Here"},"user":{"display_name":"Bob"},"inline":{"path":"a.py","to":5,"from":null}},
        {"id":2,"parent":{"id":1},"content":{"raw":"Done"},"user":{"display_name":"Ann"}},
        {"id":3,"content":{"raw":"Overall"},"user":{"display_name":"Bob"}}]}"#;
    let activity =
        r#"{"values":[{"update":{"date":"t","source":{"commit":{"hash":"b1"}}}},{"approval":{}}]}"#;
    let (_, d) = discussion_round(
        "bitbucket",
        "bitbucket.org",
        "ws/r",
        &[
            ("/comments", comments),
            ("/activity", activity),
            ("pullrequests/12", pr),
        ],
    );
    assert_eq!(d.threads.len(), 1);
    assert_eq!(d.threads[0].comments.len(), 2);
    assert_eq!(d.threads[0].line, Some(5));
    assert_eq!(d.comments.len(), 1);
    assert_eq!(d.pushes[0].commit, "b1");
}

#[test]
fn environments_need_their_permission_and_reach_cmdterm_env() {
    let source = r#"
        term.environment{
          name = "ext-test-pods",
          title = "Pods",
          program = "kubectl",
          args = { "exec", "-i", "{target}", "--", "sh", "-c", "{script}" },
          pty_args = { "exec", "-it", "{target}", "--", "sh", "-c", "{script}" },
        }
    "#;
    let mut plain = ExtensionHost::new();
    let error = plain
        .load_source_with("pods", source, Permissions::NONE)
        .unwrap_err();
    assert!(
        error.message.contains("'environments' permission"),
        "{}",
        error.message
    );
    assert!(cmdterm_env::custom_environment("ext-test-pods").is_none());

    let mut host = ExtensionHost::new();
    let granted: Permissions = [Permission::Environments].into_iter().collect();
    host.load_source_with("pods", source, granted).unwrap();
    assert_eq!(host.environments().len(), 1);
    let provider = cmdterm_env::custom_environment("ext-test-pods").unwrap();
    assert_eq!(provider.extension, "pods");
    assert_eq!(provider.title, "Pods");
    assert_eq!(provider.program, "kubectl");

    // Another extension cannot take the scheme.
    let error = host.load_source_with("thief", source, granted).unwrap_err();
    assert!(
        error.message.contains("taken by the extension 'pods'"),
        "{}",
        error.message
    );

    // Placeholders and names are checked.
    for bad in [
        r#"term.environment{ name = "ssh", program = "x", args = { "{target}" } }"#,
        r#"term.environment{ name = "ok-a", program = "x", args = { "run" } }"#,
        r#"term.environment{ name = "ok-b", program = "x", args = { "{target}", "-c={script}" } }"#,
        r#"term.environment{ name = "ok-c", program = "", args = { "{target}" } }"#,
    ] {
        assert!(
            ExtensionHost::new()
                .load_source_with("bad", bad, granted)
                .is_err(),
            "{bad}"
        );
    }

    // Dropping the extension takes its environment along.
    drop(host);
    assert!(cmdterm_env::custom_environment("ext-test-pods").is_none());
}

#[test]
fn kubernetes_runs_commands_through_kubectl_exec() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let mut host = ExtensionHost::new();
    host.turn_on(&["kubernetes".to_owned()]);
    let off: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name != "kubernetes")
        .collect();
    let grants = std::collections::BTreeMap::from([(
        "kubernetes".to_owned(),
        vec!["environments".to_owned()],
    )]);
    let values = crate::ext::SettingValues::from([(
        "kubernetes".to_owned(),
        std::collections::BTreeMap::from([(
            "namespace".to_owned(),
            crate::ext::SettingValue::String("dev".into()),
        )]),
    )]);
    let errors = host.load_dirs_except(&[&root], &off, &grants, &values);
    assert!(errors.is_empty(), "{errors:?}");
    let [k8s] = host.environments() else {
        panic!("{:?}", host.environments())
    };
    assert_eq!(k8s.scheme, "k8s");
    assert_eq!(k8s.program, "kubectl");
    assert_eq!(
        k8s.args,
        [
            "exec",
            "-i",
            "--namespace=dev",
            "{target}",
            "--",
            "sh",
            "-c",
            "{script}"
        ]
    );
    assert_eq!(k8s.pty_args.as_ref().unwrap()[1], "-it");
}

/// The shipped `name` extension alone, turned on with its permissions and
/// the setting values `values`.
fn shipped_alone(
    name: &str,
    permissions: &[&str],
    values: std::collections::BTreeMap<String, crate::ext::SettingValue>,
) -> ExtensionHost {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
    let mut host = ExtensionHost::new();
    host.turn_on(&[name.to_owned()]);
    let off: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|other| other != name)
        .collect();
    let grants = std::collections::BTreeMap::from([(
        name.to_owned(),
        permissions.iter().map(|p| (*p).to_owned()).collect(),
    )]);
    let values = crate::ext::SettingValues::from([(name.to_owned(), values)]);
    let errors = host.load_dirs_except(&[&root], &off, &grants, &values);
    assert!(errors.is_empty(), "{errors:?}");
    host
}

#[test]
fn coder_runs_commands_through_coder_ssh() {
    let host = shipped_alone("coder", &["environments"], Default::default());
    let [coder] = host.environments() else {
        panic!("{:?}", host.environments())
    };
    assert_eq!(
        (coder.scheme.as_str(), coder.program.as_str()),
        ("coder", "coder")
    );
    assert_eq!(
        coder.args,
        ["ssh", "--wait=auto", "{target}", "--", "{script}"]
    );
    assert_eq!(
        coder.pty_args.as_deref().unwrap(),
        ["ssh", "--wait=auto", "--tty", "{target}", "--", "{script}"]
    );

    let host = shipped_alone(
        "coder",
        &["environments"],
        std::collections::BTreeMap::from([
            (
                "wait".to_owned(),
                crate::ext::SettingValue::String("no".into()),
            ),
            (
                "autostart".to_owned(),
                crate::ext::SettingValue::Bool(false),
            ),
            (
                "forward_agent".to_owned(),
                crate::ext::SettingValue::Bool(true),
            ),
        ]),
    );
    assert_eq!(
        host.environments()[0].args,
        [
            "ssh",
            "--wait=no",
            "--disable-autostart",
            "--forward-agent",
            "{target}",
            "--",
            "{script}"
        ]
    );
}

#[test]
fn forges_take_the_accounts_of_the_accounts_page() {
    let source = r#"
        term.forge{
          name = "hub", title = "Hub",
          accounts = { default = { "hub.example" }, example = "hub.example/acme", hint = "A host." },
          check = function() end,
        }
    "#;
    let mut host = ExtensionHost::new();
    host.load_source_with("hub", source, Permissions::from_names(&["network"]))
        .unwrap();
    let forge = host.forges()[0].clone();
    assert_eq!(forge.hosts, ["hub.example"]);
    assert_eq!(forge.default_hosts, ["hub.example"]);
    assert_eq!(forge.example.as_deref(), Some("hub.example/acme"));
    assert_eq!(forge.hint.as_deref(), Some("A host."));

    let revision = host.forges_revision();
    let own = [("hub.hub".to_owned(), vec!["git.other.example".to_owned()])];
    host.set_forge_hosts(&own.into());
    assert_eq!(host.forges()[0].hosts, ["git.other.example"]);
    assert_ne!(host.forges_revision(), revision);
    host.set_forge_hosts(&std::collections::BTreeMap::new());
    assert_eq!(host.forges()[0].hosts, ["hub.example"]);
}

/// Loads `source` as extension `name` from a directory with `manifest`,
/// with the network and sandbox permissions, and `values` for its
/// settings. Returns the load errors.
fn load_with_manifest(
    name: &str,
    manifest: &str,
    source: &str,
    values: &crate::ext::SettingValues,
) -> (ExtensionHost, Vec<crate::ext::ExtError>) {
    let dir = tempfile::tempdir().unwrap();
    let ext = dir.path().join(name);
    std::fs::create_dir(&ext).unwrap();
    std::fs::write(ext.join("manifest.toml"), manifest).unwrap();
    std::fs::write(ext.join("init.lua"), source).unwrap();
    let mut host = ExtensionHost::new();
    let granted = [(
        name.to_owned(),
        vec!["network".to_owned(), "sandbox".to_owned()],
    )]
    .into();
    let errors = host.load_dir_except(dir.path(), &[], &granted, values);
    (host, errors)
}

#[test]
fn http_reaches_only_the_extensions_hosts() {
    let manifest = r#"
permissions = ["network"]
hosts = ["api.example.com"]

[[settings]]
key = "mirrors"
type = "hosts"

[[settings]]
key = "lists"
type = "urls"
"#;
    let source = r#"
        term.forge{ name = "hub", accounts = { default = { "hub.example/acme" } }, check = function() end }
        for _, url in ipairs({
          "https://api.example.com/x", "https://hub.example/api/v1/user",
          "https://api.hub.example/graphql", "https://mirror.example/",
          "https://lists.example/b.txt", "https://evil.example/", "https://example.com/",
          "https://nothub.example/",
        }) do
          local ok = pcall(term.http, { url = url, done = function() end })
          term.notify(url .. " " .. tostring(ok))
        end
    "#;
    let mut own = std::collections::BTreeMap::new();
    own.insert(
        "mirrors".to_owned(),
        crate::ext::SettingValue::List(vec!["Mirror.example".into()]),
    );
    own.insert(
        "lists".to_owned(),
        crate::ext::SettingValue::List(vec!["https://lists.example:8443/a.txt".into()]),
    );
    let values = [("net".to_owned(), own)].into();
    let (mut host, errors) = load_with_manifest("net", manifest, source, &values);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        notes(&mut host),
        [
            "https://api.example.com/x true",
            "https://hub.example/api/v1/user true",
            "https://api.hub.example/graphql true",
            "https://mirror.example/ true",
            "https://lists.example/b.txt true",
            "https://evil.example/ false",
            "https://example.com/ false",
            "https://nothub.example/ false",
        ]
    );
    assert!(host.allows_host("net", "api.example.com"));
    assert!(!host.allows_host("net", "evil.example"));
    assert!(!host.allows_host("nobody", "api.example.com"));
}

#[test]
fn blocklists_download_only_from_the_extensions_hosts() {
    let manifest = "permissions = [\"sandbox\", \"network\"]\nhosts = [\"lists.example\"]\n";
    let values = crate::ext::SettingValues::new();
    let good =
        r#"term.sandbox{ blocklists = { { name = "a", urls = { "https://lists.example/a" } } } }"#;
    let (_, errors) = load_with_manifest("bl", manifest, good, &values);
    assert!(errors.is_empty(), "{errors:?}");
    let bad =
        r#"term.sandbox{ blocklists = { { name = "a", urls = { "https://other.example/a" } } } }"#;
    let (_, errors) = load_with_manifest("bl", manifest, bad, &values);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not on one of the extension's hosts")),
        "{errors:?}"
    );
}

#[test]
fn shipped_forges_guard_change_requests_on_their_accounts() {
    use cmdterm_env::{GuardKind, ProxyRules};
    let mut host = shipped_alone("github", &["network", "guards"], Default::default());
    let rules = host.sandbox_rules().clone();
    let kinds: Vec<(&str, Option<GuardKind>)> = (rules.guards.iter())
        .map(|g| (g.id.as_str(), g.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("github.create", Some(GuardKind::Create)),
            ("github.comment", Some(GuardKind::Comment)),
            ("github.merge", Some(GuardKind::Merge)),
        ]
    );
    let proxy = ProxyRules {
        secrets: vec![],
        guards: rules.guards.clone(),
    };
    let held = |host: &str, method: &str, target: &str, body: &str| {
        (proxy.guard(host, method, target))
            .or_else(|| proxy.body_guard(host, method, target, Some(body.as_bytes())))
            .map(|g| g.id.clone())
    };
    assert_eq!(
        held("api.github.com", "POST", "/repos/o/r/pulls", "").as_deref(),
        Some("github.create")
    );
    let mutation =
        |name: &str| format!(r#"{{"query":"mutation M($i:I!){{{name}(input:$i){{x}}}}"}}"#);
    for (name, id) in [
        ("createPullRequest", "github.create"),
        ("addComment", "github.comment"),
        ("addPullRequestReview", "github.comment"),
        ("mergePullRequest", "github.merge"),
        ("enablePullRequestAutoMerge", "github.merge"),
    ] {
        assert_eq!(
            held("api.github.com", "POST", "/graphql", &mutation(name)).as_deref(),
            Some(id),
            "{name}"
        );
    }
    assert_eq!(
        held(
            "api.github.com",
            "POST",
            "/graphql",
            r#"{"query":"query{viewer{login}}"}"#
        ),
        None
    );
    assert_eq!(
        held("api.github.com", "PUT", "/repos/o/r/pulls/3/merge", "").as_deref(),
        Some("github.merge")
    );
    // Only the accounts' hosts: another host's API is left alone.
    assert_eq!(held("example.org", "POST", "/repos/o/r/pulls", ""), None);
    // An Enterprise account brings its host.
    host.set_forge_hosts(&[("github.github".to_owned(), vec!["ghe.example".into()])].into());
    let guard = host.sandbox_rules().guards[0].clone();
    assert!(
        guard.hosts.contains(&"ghe.example".to_owned()),
        "{:?}",
        guard.hosts
    );
    assert!(!guard.hosts.contains(&"github.com".to_owned()));
}

#[test]
fn every_shipped_forge_guards_create_comment_and_merge() {
    for name in ["gitlab", "bitbucket", "forgejo", "gitea", "azure-devops"] {
        let host = shipped_alone(name, &["network", "guards"], Default::default());
        let kinds: Vec<_> = host.sandbox_rules().guards.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds,
            [
                Some(cmdterm_env::GuardKind::Create),
                Some(cmdterm_env::GuardKind::Comment),
                Some(cmdterm_env::GuardKind::Merge)
            ],
            "{name}"
        );
        // Without the permission the forge still loads, without guards.
        let host = shipped_alone(name, &["network"], Default::default());
        assert!(host.sandbox_rules().guards.is_empty(), "{name}");
        assert_eq!(host.forges().len(), 1, "{name}");
    }
    let azure = shipped_alone("azure-devops", &["network", "guards"], Default::default());
    let proxy = cmdterm_env::ProxyRules {
        secrets: vec![],
        guards: azure.sandbox_rules().guards.clone(),
    };
    let target = "/org/proj/_apis/git/repositories/r/pullRequests/7?api-version=7.1";
    let complete = proxy.body_guard(
        "dev.azure.com",
        "PATCH",
        target,
        Some(br#"{"status": "completed", "lastMergeSourceCommit": {}}"#),
    );
    assert_eq!(complete.map(|g| g.id.as_str()), Some("azure-devops.merge"));
    let title = proxy.body_guard("dev.azure.com", "PATCH", target, Some(br#"{"title":"x"}"#));
    assert!(title.is_none());
    assert!(proxy.wants_body("org.visualstudio.com", "PATCH", target));
}

#[test]
fn agents_need_their_permissions_and_are_checked() {
    let source = r#"
        term.agent{
          name = "ext-test-coder",
          title = "Test Coder",
          config_dir = "~/.ext-test-coder",
          skills_dir = ".ext-test-coder/skills",
          integration = {
            file = ".ext-test-coder/settings.json",
            hooks = {
              { event = "SessionStart", args = "context ext-test-coder" },
              { event = "PreToolUse", matcher = "Ask", args = "input Question" },
              { event = "Notification", args = "permission 'Needs you'" },
              { event = "Stop", args = "stop" },
            },
            timeout_unit = 1000,
          },
          mcp = { file = ".ext-test-coder/mcp.json", style = "command" },
          supports = { "state", "session", "resume", "prompt" },
          resume = { "--resume" },
          prompt = { "--ask", "{prompt}" },
          session_at_start = false,
          sessions = { dir = "~/.ext-test-coder/chats", by = "contents" },
          npm = { "@example/test-coder" },
        }
    "#;
    let error = ExtensionHost::new()
        .load_source_with("coder", source, Permissions::NONE)
        .unwrap_err();
    assert!(
        error.message.contains("'agents' permission"),
        "{}",
        error.message
    );

    let agents: Permissions = [Permission::Agents].into_iter().collect();
    let mut host = ExtensionHost::new();
    host.load_source_with("coder", source, agents).unwrap();
    let [agent] = host.agents() else {
        panic!("{:?}", host.agents());
    };
    assert_eq!(agent.extension, "coder");
    assert_eq!(agent.program, "ext-test-coder");
    assert_eq!(agent.config_dir, ".ext-test-coder");
    assert_eq!(agent.resume.as_deref(), Some(&["--resume".to_owned()][..]));
    assert!(!agent.session_at_start);
    use crate::agents::Capability;
    assert_eq!(
        agent.supports,
        [
            Capability::State,
            Capability::Session,
            Capability::Resume,
            Capability::Prompt
        ]
    );
    assert_eq!(
        agent.prompt.as_deref(),
        Some(&["--ask".to_owned(), "{prompt}".to_owned()][..])
    );
    assert_eq!(
        agent.sessions,
        Some(crate::ext::AgentSessions {
            dir: ".ext-test-coder/chats".to_owned(),
            by_name: false,
        })
    );
    let crate::ext::AgentIntegration::Hooks {
        hooks,
        timeout_unit,
        ..
    } = &agent.integration
    else {
        panic!("{:?}", agent.integration);
    };
    assert_eq!((hooks.len(), *timeout_unit), (4, 1000));
    assert_eq!(hooks[1].matcher.as_deref(), Some("Ask"));

    // Another extension cannot take the name.
    let error = host.load_source_with("thief", source, agents).unwrap_err();
    assert!(
        error.message.contains("taken by the extension 'coder'"),
        "{}",
        error.message
    );

    // Sandbox paths need the sandbox permission too.
    let sandboxed = r#"term.agent{ name = "ext-test-boxed", config_dir = ".boxed",
        sandbox = { writable = { "~/.boxed" }, synced = { ".boxed/s.json" },
                    guarded = { [".boxed/s.json"] = { { "trust" } } } } }"#;
    let error = ExtensionHost::new()
        .load_source_with("boxed", sandboxed, agents)
        .unwrap_err();
    assert!(
        error.message.contains("'sandbox' permission"),
        "{}",
        error.message
    );
    let both: Permissions = [Permission::Agents, Permission::Sandbox]
        .into_iter()
        .collect();
    let mut boxed = ExtensionHost::new();
    boxed.load_source_with("boxed", sandboxed, both).unwrap();
    let sandbox = &boxed.agents()[0].sandbox;
    assert_eq!(sandbox.writable, [".boxed"]);
    assert_eq!(
        sandbox.guarded,
        [(".boxed/s.json".to_owned(), vec![vec!["trust".to_owned()]])]
    );

    // Login variables reach sandboxes, so they need the env permission
    // too, and must be variable names.
    let login = r#"term.agent{ name = "ext-test-login", config_dir = ".login",
        sandbox = { login_vars = { "LOGIN_AGENT_API_KEY" } } }"#;
    let error = ExtensionHost::new()
        .load_source_with("login", login, both)
        .unwrap_err();
    assert!(
        error.message.contains("'env' permission"),
        "{}",
        error.message
    );
    let all: Permissions = [Permission::Agents, Permission::Sandbox, Permission::Env]
        .into_iter()
        .collect();
    let mut logged = ExtensionHost::new();
    logged.load_source_with("login", login, all).unwrap();
    assert_eq!(
        logged.agents()[0].sandbox.login_vars,
        ["LOGIN_AGENT_API_KEY"]
    );
    for bad in [r#"{ "A B" }"#, r#"{ "1KEY" }"#, r#"{ "" }"#, r#""KEY""#] {
        let source = format!(
            r#"term.agent{{ name = "ext-test-bad-login", config_dir = ".bad",
                sandbox = {{ login_vars = {bad} }} }}"#
        );
        let error = ExtensionHost::new()
            .load_source_with("bad-login", &source, all)
            .unwrap_err();
        assert!(
            error.message.contains("login_vars"),
            "{bad}: {}",
            error.message
        );
    }

    // Hooks that provide what the agent opted in to pass.
    let messaging = r#"term.agent{ name = "ext-test-talker", config_dir = ".talker",
        supports = { "state", "session", "messages", "paste" },
        integration = { file = ".talker/h.json", hooks = {
            { event = "Start", args = "context ext-test-talker" },
            { event = "Prompt", args = "deliver Prompt" },
            { event = "Stop", args = "stop" } } } }"#;
    ExtensionHost::new()
        .load_source_with("talker", messaging, agents)
        .unwrap();

    // Whatever reaches a shell or the home directory is checked.
    for bad in [
        r#"term.agent{ name = "Bad", config_dir = ".x" }"#,
        r#"term.agent{ name = "ok-a" }"#,
        r#"term.agent{ name = "ok-b", config_dir = "/etc" }"#,
        r#"term.agent{ name = "ok-c", config_dir = ".x/../.ssh" }"#,
        r#"term.agent{ name = "ok-d", config_dir = ".x", program = "rm -rf" }"#,
        r#"term.agent{ name = "ok-e", config_dir = ".x", program = "-x" }"#,
        r#"term.agent{ name = "ok-f", config_dir = ".x", resume = { "$(id)" } }"#,
        r#"term.agent{ name = "ok-g", config_dir = ".x",
            integration = { file = ".x/h.json", hooks = { { event = "S", args = "busy; id" } } } }"#,
        r#"term.agent{ name = "ok-h", config_dir = ".x",
            integration = { file = ".x/h.json", hooks = { { event = "S", args = "rm -rf" } } } }"#,
        r#"term.agent{ name = "ok-i", config_dir = ".x", integration = { file = ".x/h.json" } }"#,
        r#"term.agent{ name = "ok-j", config_dir = ".x", mcp = { file = ".x/m.json", style = "toml" } }"#,
        r#"term.agent{ name = "ok-k", config_dir = ".x", supports = { "session", "resume" },
            resume = { "-r" }, sessions = { dir = "$HOME/s" } }"#,
        r#"term.agent{ name = "ok-l", config_dir = ".x", supports = { "session", "resume" },
            resume = { "-r" }, sessions = { dir = ".x/s", by = "glob" } }"#,
        r#"term.agent{ name = "ok-m", config_dir = ".x", supports = { "session", "resume" },
            resume = { "-r" }, sessions = { by = "name" } }"#,
        // Opting in to what it cannot do, or not to what it does.
        r#"term.agent{ name = "ok-n", config_dir = ".x", supports = { "teleport" } }"#,
        r#"term.agent{ name = "ok-o", config_dir = ".x", resume = { "-r" } }"#,
        r#"term.agent{ name = "ok-p", config_dir = ".x", supports = { "session", "resume" } }"#,
        r#"term.agent{ name = "ok-q", config_dir = ".x", supports = { "prompt" } }"#,
        r#"term.agent{ name = "ok-r", config_dir = ".x", prompt = { "{prompt}" } }"#,
        r#"term.agent{ name = "ok-s", config_dir = ".x", supports = { "prompt" }, prompt = { "-p" } }"#,
        r#"term.agent{ name = "ok-t", config_dir = ".x", supports = { "paste" } }"#,
        r#"term.agent{ name = "ok-u", config_dir = ".x", supports = { "session", "paste" },
            session_at_start = false }"#,
        r#"term.agent{ name = "ok-v", config_dir = ".x", supports = { "messages" } }"#,
        r#"term.agent{ name = "ok-w", config_dir = ".x", supports = { "state" },
            integration = { file = ".x/h.json", hooks = { { event = "S", args = "context ok-w" } } } }"#,
        r#"term.agent{ name = "ok-x", config_dir = ".x", supports = { "state", "session", "messages" },
            integration = { file = ".x/h.json", hooks = { { event = "S", args = "context ok-x" },
                                                       { event = "T", args = "stop" } } } }"#,
    ] {
        assert!(
            ExtensionHost::new()
                .load_source_with("bad", bad, agents)
                .is_err(),
            "{bad}"
        );
    }
}
