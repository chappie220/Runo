use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Action {
    app.key(KeyEvent::new(code, modifiers))
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

fn app() -> App {
    App::new(Lang::En, "Claw".into(), "x/y".into(), "main".into())
}

fn session(name: &str, messages: i64) -> SessionSummary {
    SessionSummary {
        name: name.into(),
        messages,
        updated_at: 0,
    }
}

/// The screen as text rows.
fn screen(app: &App, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect();
    (rows, terminal)
}

fn plain(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .map(|s| s.trim_end().to_owned())
        .collect()
}

#[test]
fn lays_out_sidebar_panel_and_footer_and_hides_the_list_when_narrow() {
    let mut app = app();
    app.sessions = vec![session("main", 4), session("work", 12)];
    app.context = Some((12_000, 200_000));
    let (rows, mut terminal) = screen(&app, 100, 12);
    assert!(rows[0].starts_with("╭ Sessions"), "{rows:#?}");
    assert!(rows[0].contains("Claw · main"), "{rows:#?}");
    assert!(rows[0].trim_end().ends_with("x/y ╮"), "{rows:#?}");
    assert!(rows[1].starts_with("│▸ main"), "{rows:#?}");
    assert!(
        rows[2].starts_with("│  work")
            && rows[2].chars().take(26).collect::<String>().contains("12"),
        "{rows:#?}"
    );
    // The input sits under a separator that joins the border.
    assert!(rows[8].contains("├──"), "{rows:#?}");
    assert!(rows[9].contains("› Message Claw"), "{rows:#?}");
    assert!(
        rows[11].contains("context 12k / 200k ▰▱▱▱▱▱▱▱ 6%"),
        "{rows:#?}"
    );
    // The cursor waits after the prompt.
    terminal.backend_mut().assert_cursor_position((30, 9));

    let (rows, _) = screen(&app, 50, 10);
    assert!(
        rows[0].starts_with("╭ ✻ Claw"),
        "narrow: no list\n{rows:#?}"
    );
    app.input.set("/x");
    let (rows, mut terminal) = screen(&app, 50, 10);
    assert!(rows[7].starts_with("│ › /x"), "{rows:#?}");
    terminal.backend_mut().assert_cursor_position((6, 7));
}

#[test]
fn edits_the_input_line() {
    let mut app = app();
    typed(&mut app, "hello world");
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    typed(&mut app, "there");
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    press(&mut app, KeyCode::Delete, KeyModifiers::NONE);
    typed(&mut app, "H");
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    typed(&mut app, "x");
    assert_eq!(app.input.string(), "Hello there\nx");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.input.string(), "Hello therex");
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::Send
    );
}

#[test]
fn keys_stop_a_reply_and_quit_when_idle() {
    let mut app = app();
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::None
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
        Action::Quit
    );
    app.running = true;
    assert_eq!(
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE),
        Action::Stop
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
        Action::Stop
    );
    // Ctrl+D asks again while a reply runs.
    assert_eq!(
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL),
        Action::None
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL),
        Action::Quit
    );
}

#[test]
fn approvals_take_y_or_n() {
    let mut app = app();
    let (reply, answer) = oneshot::channel();
    app.approval = Some(("shell".into(), "rm -rf build".into(), reply));
    typed(&mut app, "x");
    assert!(app.approval.is_some(), "other keys wait");
    typed(&mut app, "y");
    assert!(app.approval.is_none());
    assert_eq!(answer.blocking_recv(), Ok(true));
    // The tool line shows it ran; only a refusal is noted.
    assert!(app.entries.is_empty());
    let (reply, answer) = oneshot::channel();
    app.approval = Some(("shell".into(), "rm -rf build".into(), reply));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(answer.blocking_recv(), Ok(false));
    assert_eq!(
        app.entries.last(),
        Some(&Entry::Note("Denied shell.".into()))
    );
}

#[test]
fn recalls_earlier_messages() {
    let mut app = app();
    app.sent = vec!["one".into(), "two".into()];
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.string(), "one");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.string(), "two");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.input.is_empty());
}

#[test]
fn streams_markdown_and_tools_open_to_show_their_output() {
    let mut app = app();
    app.on_agent(AgentEvent::ToolStart {
        name: "shell".into(),
        arguments: "{\"command\":\n\"ls\"}".into(),
    });
    let (lines, _) = app.lines(40);
    assert!(
        plain(&lines)[1].starts_with("  ⠋ shell {\"command\": \"ls\"}"),
        "{lines:?}"
    );
    app.on_agent(AgentEvent::ToolEnd {
        name: "shell".into(),
        output: "a\nb".into(),
    });
    app.on_agent(AgentEvent::Text("**Hel".into()));
    app.on_agent(AgentEvent::Text("lo**".into()));
    app.finished(Ok("**Hello**".into()));
    assert!(matches!(
        &app.entries[0],
        Entry::Tool { output: Some(o), elapsed: Some(_), expanded: false, .. } if o == "a\nb"
    ));
    assert_eq!(app.entries[1], Entry::Assistant("**Hello**".into()));
    let (lines, owners) = app.lines(40);
    let rows = plain(&lines);
    assert_eq!(rows[0], "✻ Claw");
    assert!(
        rows[1].starts_with("  ▸ shell") && rows[1].contains("✓"),
        "{rows:?}"
    );
    // A blank line between the tool and the text.
    assert_eq!(rows[2..4], ["", "  Hello"]);
    assert!(
        lines[3].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(owners[..4], [None, Some(0), None, None]);

    // Ctrl+O opens every tool's output; again closes it.
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let rows = plain(&app.lines(40).0);
    assert!(rows[1].starts_with("  ▾ shell"), "{rows:?}");
    assert_eq!(rows[2..4], ["    │ a", "    │ b"]);
    press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(plain(&app.lines(40).0)[3], "  Hello");

    // A reply that did not stream still shows.
    app.entries.push(Entry::User("again".into()));
    app.finished(Ok("Stopped.".into()));
    assert_eq!(
        app.entries.last(),
        Some(&Entry::Assistant("Stopped.".into()))
    );
}

#[test]
fn a_click_opens_a_tool_and_another_opens_a_session() {
    let mut app = app();
    app.sessions = vec![session("main", 1), session("work", 3)];
    app.on_agent(AgentEvent::ToolStart {
        name: "shell".into(),
        arguments: "ls".into(),
    });
    app.on_agent(AgentEvent::ToolEnd {
        name: "shell".into(),
        output: "file".into(),
    });
    let (rows, _) = screen(&app, 100, 12);
    let tool_row = rows.iter().position(|r| r.contains("▸ shell")).unwrap() as u16;
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(app.mouse(click(40, tool_row)), Action::None);
    assert!(matches!(app.entries[0], Entry::Tool { expanded: true, .. }));
    assert_eq!(app.mouse(click(5, 2)), Action::Run("/session work".into()));
    // The open session is not opened again.
    assert_eq!(app.mouse(click(5, 1)), Action::None);
    let wheel = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        ..click(40, 5)
    };
    app.mouse(wheel);
    assert_eq!(app.scroll, WHEEL_LINES);
}

#[test]
fn tab_moves_to_the_session_list_and_enter_opens_one() {
    let mut app = app();
    app.sessions = vec![session("work", 3), session("main", 1), session("old", 9)];
    screen(&app, 100, 12);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Sidebar);
    assert_eq!(app.sidebar_selected, 1, "starts on the open session");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.sidebar_selected, 2);
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::Run("/session old".into())
    );
    assert_eq!(app.focus, Focus::Input);
    // Typing from the list goes to the input.
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    typed(&mut app, "hi");
    assert_eq!(
        (app.focus, app.input.string().as_str()),
        (Focus::Input, "hi")
    );
    // Ctrl+B hides the list; then Tab stays in the input.
    press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    screen(&app, 100, 12);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Input);
    assert_eq!(
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL),
        Action::Run("/new".into())
    );
}

#[test]
fn slash_lists_commands_and_enter_runs_one() {
    let mut app = app();
    typed(&mut app, "/se");
    let c = app.completion.as_ref().expect("/ opens the command list");
    assert_eq!(c.kind, Kind::Command);
    let names: Vec<&str> = c.items.iter().map(|(n, _)| n.as_str()).collect();
    // Exact starts first, then letters in order.
    assert_eq!(names, ["session", "sessions", "setup", "sidebar"]);
    // A command that takes an argument waits for it.
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::None
    );
    assert_eq!(app.input.string(), "/session ");
    assert!(app.completion.is_none());
    app.input.take();
    typed(&mut app, "/skl");
    assert_eq!(app.completion.as_ref().unwrap().items[0].0, "skills");
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::Run("/skills".into())
    );
    assert!(app.input.is_empty());
    // Past the first word the list closes; a path is not a command.
    typed(&mut app, "/attach ./a");
    assert!(app.completion.is_none());
    app.input.take();
    typed(&mut app, "see /etc");
    assert!(app.completion.is_none());
}

#[test]
fn ctrl_k_finds_commands_and_sessions() {
    let mut app = app();
    app.sessions = vec![session("main", 1), session("work", 3)];
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert!(app.palette.is_some());
    typed(&mut app, "wo");
    let labels: Vec<String> = app
        .palette_items("wo")
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert_eq!(labels, ["work"]);
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::Run("/session work".into())
    );
    assert!(app.palette.is_none());
    // A command that needs an argument goes to the input to finish.
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
    typed(&mut app, "attach");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.input.string(), "/attach ");
    // Esc closes it without touching the input.
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
    typed(&mut app, "zz");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.palette.is_none());
    assert_eq!(app.input.string(), "/attach ");
    let (rows, _) = screen(&app, 80, 20);
    assert!(!rows.iter().any(|r| r.contains("Commands")));
}

#[test]
fn lays_out_wide_text_and_the_cursor() {
    assert_eq!(wrap("one two three", 8), ["one two", "three"]);
    assert_eq!(wrap("中文中文中文", 4), ["中文", "中文", "中文"]);
    assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    assert_eq!(one_line("a\n  b   c", 10), "a b c");
    assert_eq!(one_line("abcdefgh", 5), "abcd…");
    let mut input = Input::default();
    input.set("中文ab\ncd");
    let (rows, cursor) = input.layout(4);
    assert_eq!(rows, ["中文", "ab", "cd"]);
    assert_eq!(cursor, (2, 2));
    input.cursor = 2;
    assert_eq!(input.layout(4).1, (1, 0));
    let items = ["sessions", "session", "skills", "expand"];
    assert_eq!(
        ranked(&items, "ss", |s| s),
        [&"sessions", &"session", &"skills"]
    );
}

fn with_skills(names: &[&str]) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    for name in names {
        let file = dir.path().join("skills").join(name).join("SKILL.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            format!("---\nname: {name}\ndescription: Does {name} things\n---\nSteps.\n"),
        )
        .unwrap();
    }
    let mut app = app();
    app.skills = Some(Skills::new(
        dir.path(),
        &crate::config::SkillsConfig::default(),
    ));
    (dir, app)
}

#[test]
fn at_lists_skills_and_tab_completes_one() {
    let (_dir, mut app) = with_skills(&["weather", "web-research", "deploy"]);
    typed(&mut app, "look up @");
    let c = app.completion.as_ref().expect("the list opens on @");
    let names: Vec<&str> = c.items.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["deploy", "weather", "web-research"]);
    typed(&mut app, "we");
    let names: Vec<&str> = app
        .completion
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names, ["weather", "web-research"]);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.input.string(), "look up @web-research ");
    assert!(app.completion.is_none());
    // Esc closes the list for that @ only; Enter still sends.
    typed(&mut app, "@x");
    assert!(app.completion.is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    typed(&mut app, "y");
    assert!(app.completion.is_none());
    assert_eq!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        Action::Send
    );
    // An email address is not a mention.
    typed(&mut app, " me@we");
    assert!(app.completion.is_none());
    assert_eq!(
        app.mentioned_skills("use @weather, then @deploy and @nope @weather"),
        ["weather", "deploy"]
    );
}

#[test]
fn the_at_list_says_why_it_is_empty() {
    let mut app = app();
    typed(&mut app, "@");
    let c = app.completion.as_ref().unwrap();
    assert_eq!(c.items[0].1, SKILLS_OFF.get(Lang::En));
    // Enter on the note sends nothing in and closes it.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.completion.is_none());
    assert_eq!(app.input.string(), "@");
    let (_dir, mut app) = with_skills(&[]);
    typed(&mut app, "@");
    assert_eq!(
        app.completion.as_ref().unwrap().items[0].1,
        NO_SKILLS.get(Lang::En)
    );
}

#[test]
fn every_text_has_both_languages() {
    crate::i18n::assert_complete(ALL);
    crate::i18n::assert_complete(manage::ALL);
}

#[test]
fn a_model_outage_opens_the_management_view_and_its_menu_works() {
    let mut app = app();
    app.model_down("model request failed: connection refused".into());
    assert!(app.manage.is_some());
    assert_eq!(app.focus, Focus::Manage);
    let (rows, _) = screen(&app, 100, 30);
    let text = rows.join("\n");
    assert!(text.contains("✗ x/y"), "{text}");
    assert!(
        text.contains("Model x/y: unavailable: model request failed"),
        "{text}"
    );
    assert!(text.contains("1 Check the model again"), "{text}");
    assert!(text.contains("Esc back to the chat"), "{text}");

    // ↓ and Enter pick an item; digits too.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let (title, rows) = app.manage.as_ref().unwrap().detail.clone().unwrap();
    assert_eq!(title, "System status");
    assert!(rows.iter().any(|r| r.starts_with("Memory: ")), "{rows:?}");
    press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
    assert!(app.check_model);
    assert_eq!(app.health, Health::Checking);
    press(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
    let (title, rows) = app.manage.as_ref().unwrap().detail.clone().unwrap();
    assert_eq!(title, "Configuration changes");
    assert!(
        rows.iter().any(|r| r.contains("runo changes revert")),
        "{rows:?}"
    );
    press(&mut app, KeyCode::Char('6'), KeyModifiers::NONE);
    assert!(app.open_shell);
    press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
    assert!(app.change_model);

    // Typing goes to the input, so !shell works from here; Tab comes back.
    typed(&mut app, "!shell");
    assert_eq!(app.focus, Focus::Input);
    assert_eq!(app.input.string(), "!shell");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Manage);

    // Esc goes back to the chat.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.manage.is_none());
    assert_eq!(app.focus, Focus::Input);
}

#[test]
fn the_model_is_not_changed_during_a_reply() {
    let mut app = app();
    app.open_manage();
    app.running = true;
    press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
    assert!(!app.change_model);
}

#[test]
fn only_a_bare_bang_shell_opens_the_shell() {
    assert!(is_shell_command("!shell"));
    assert!(is_shell_command("  !shell \n"));
    assert!(!is_shell_command("!shell ls"));
    assert!(!is_shell_command("!Shell"));
    assert!(!is_shell_command("please run !shell"));
    assert!(!is_shell_command("/shell"));
}

#[test]
fn bang_shell_runs_a_shell_with_the_signals_given_back() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("sh");
    let out = dir.path().join("out");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$RUNO_SHELL\" > {}\ngrep -E '^SigIgn' /proc/self/status >> {}\nexit 3\n",
            out.display(),
            out.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = run_shell(script.to_str().unwrap(), None).unwrap();
    assert_eq!(status.code(), Some(3));
    let written = std::fs::read_to_string(&out).unwrap();
    let mut lines = written.lines();
    assert_eq!(lines.next(), Some("1"));
    // SIGINT (bit 1) and SIGQUIT (bit 2) are not ignored in the shell.
    let mask =
        u64::from_str_radix(lines.next().unwrap().split_whitespace().nth(1).unwrap(), 16).unwrap();
    assert_eq!(mask & 0b110, 0);
}

#[test]
fn setup_sends_the_skill_and_starts_by_itself_only_once() {
    let mut app = app();
    // Without skills it says what is missing and sends nothing.
    assert!(app.setup().is_none());
    assert!(matches!(app.entries.last(), Some(Entry::Note(n)) if n.contains("skills.enabled")));

    let dir = tempfile::tempdir().unwrap();
    app.skills = Some(Skills::new(
        dir.path(),
        &crate::config::SkillsConfig::default(),
    ));
    app.state = dir.path().to_owned();
    let sent = app.setup().unwrap();
    assert!(sent.text.starts_with("Check and set up this device."));
    assert!(sent.text.contains("Skills to use: system-setup"));
    assert!(
        matches!(app.entries.last(), Some(Entry::User(t)) if t == "Check and set up this device.")
    );

    // The first start waits while the model is down, then goes once.
    app.setup_due = true;
    app.model_down("no network".into());
    assert!(app.first_setup().is_none());
    assert!(app.setup_due);
    app.health = Health::Up("ok".into());
    let sent = app.first_setup().unwrap();
    assert!(sent.text.contains("system-setup"));
    assert!(app.manage.is_none());
    assert_eq!(app.focus, Focus::Input);
    assert!(dir.path().join(SETUP_MARK).exists());
    assert!(app.first_setup().is_none());

    // A disabled skill is not started.
    let config = crate::config::SkillsConfig {
        disabled: vec!["system-setup".into()],
        ..Default::default()
    };
    app.skills = Some(Skills::new(dir.path(), &config));
    assert!(app.setup().is_none());
}
