use zellij_tile::prelude::*;

use chrono::Local;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use uuid::Uuid;

use zjstatus::{
    config::{self, ModuleConfig, UpdateEventMask, ZellijState},
    frames, pipe,
    widgets::{
        command::{CommandResult, CommandWidget},
        datetime::DateTimeWidget,
        mode::ModeWidget,
        notification::NotificationWidget,
        pipe::PipeWidget,
        session::SessionWidget,
        swap_layout::SwapLayoutWidget,
        tabs::TabsWidget,
        widget::Widget,
    },
};

// Matches the old incidental Zellij session scan cadence.
const REFRESH_INTERVAL_SECONDS: f64 = 1.0;

#[derive(Default)]
struct State {
    pending_events: Vec<Event>,
    got_permissions: bool,
    state: ZellijState,
    userspace_configuration: BTreeMap<String, String>,
    module_config: config::ModuleConfig,
    widget_map: BTreeMap<String, Arc<dyn Widget>>,
    focus_cwd_commands: Vec<String>,
    err: Option<anyhow::Error>,
    pane_cwds: BTreeMap<PaneId, PathBuf>,
}

#[cfg(not(test))]
register_plugin!(State);

#[cfg(feature = "tracing")]
fn init_tracing() {
    use std::fs::File;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let file = File::create("/host/.zjstatus.log");
    let file = match file {
        Ok(file) => file,
        Err(error) => panic!("Error: {:?}", error),
    };
    let debug_log = tracing_subscriber::fmt::layer().with_writer(Arc::new(file));

    tracing_subscriber::registry().with(debug_log).init();

    tracing::info!("tracing initialized");
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        #[cfg(feature = "tracing")]
        init_tracing();

        self.pending_events = Vec::new();
        self.got_permissions = false;
        self.err = None;
        self.module_config = match ModuleConfig::new(&configuration) {
            Ok(mc) => mc,
            Err(e) => {
                self.err = Some(e);
                ModuleConfig::default()
            }
        };

        // we need the ReadApplicationState permission to receive the ModeUpdate and TabUpdate
        // events
        // we need the RunCommands permission to run "cargo test" in a floating window
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
        ]);

        subscribe(&subscribed_events(&self.frame_config()));
        set_timeout(REFRESH_INTERVAL_SECONDS);

        if self.err.is_some() {
            return;
        }

        self.widget_map = register_widgets(&configuration);
        self.focus_cwd_commands =
            zjstatus::widgets::command::focus_cwd_command_names(&configuration);
        self.userspace_configuration = configuration;
        let uid = Uuid::new_v4();

        self.state = ZellijState {
            cols: 0,
            command_results: BTreeMap::new(),
            pipe_results: BTreeMap::new(),
            mode: ModeInfo::default(),
            panes: PaneManifest::default(),
            plugin_uuid: uid.to_string(),
            tabs: Vec::new(),
            sessions: Vec::new(),
            start_time: Local::now(),
            cache_mask: 0,
            incoming_notification: None,
            focused_pane_id: None,
            focused_pane_cwd: None,
            command_epochs: BTreeMap::new(),
        };
        self.pane_cwds = BTreeMap::new();
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        let mut should_render = false;
        if !self.focus_cwd_commands.is_empty() && self.state.focused_pane_cwd.is_none() {
            should_render |= self.update_focused_pane();
        }

        match pipe_message.source {
            PipeSource::Cli(_) => {
                if let Some(input) = pipe_message.payload {
                    should_render |= pipe::parse_protocol(&mut self.state, &input);
                }
            }
            PipeSource::Plugin(_) => {
                if let Some(input) = pipe_message.payload {
                    should_render |= pipe::parse_protocol(&mut self.state, &input);
                }
            }
            PipeSource::Keybind => {
                if let Some(input) = pipe_message.payload {
                    should_render |= pipe::parse_protocol(&mut self.state, &input);
                }
            }
        }

        should_render
    }

    #[tracing::instrument(skip_all, fields(event_type))]
    fn update(&mut self, event: Event) -> bool {
        if let Event::PermissionRequestResult(PermissionStatus::Granted) = event {
            self.got_permissions = true;
            let mut should_render = false;

            for ev in std::mem::take(&mut self.pending_events) {
                tracing::debug!("processing cached event");
                should_render |= self.handle_event(ev);
            }

            should_render |= self.handle_event(event);
            return should_render;
        }

        if !self.got_permissions {
            tracing::debug!("caching event");
            self.pending_events.push(event);

            return false;
        }

        self.handle_event(event)
    }

    #[tracing::instrument(skip_all)]
    fn render(&mut self, _rows: usize, cols: usize) {
        if !self.got_permissions {
            return;
        }

        if let Some(err) = &self.err {
            println!("Error: {:?}", err);

            return;
        }

        self.state.cols = cols;

        tracing::debug!("{:?}", self.state.mode.session_name);

        let output = self
            .module_config
            .render_bar(self.state.clone(), self.widget_map.clone());

        print!("{}", output);
    }
}

#[cfg(not(test))]
fn query_pane_cwd(pane_id: PaneId) -> std::result::Result<PathBuf, String> {
    get_pane_cwd(pane_id)
}

#[cfg(test)]
fn query_pane_cwd(_pane_id: PaneId) -> std::result::Result<PathBuf, String> {
    Err("not available in unit tests".to_owned())
}

impl State {
    fn frame_config(&self) -> frames::FrameConfig {
        frames::FrameConfig::new(
            self.module_config.hide_frame_for_single_pane,
            self.module_config.hide_frame_except_for_search,
            self.module_config.hide_frame_except_for_fullscreen,
            self.module_config.hide_frame_except_for_scroll,
            &self.module_config.pane_frame_style,
        )
    }

    fn update_focused_pane(&mut self) -> bool {
        self.update_focused_pane_with(query_pane_cwd)
    }

    fn update_focused_pane_with(
        &mut self,
        fetch_cwd: impl FnOnce(PaneId) -> std::result::Result<PathBuf, String>,
    ) -> bool {
        let active_tab = self.state.tabs.iter().find(|t| t.active);

        let new_id = active_tab
            .and_then(|tab| self.state.panes.panes.get(&tab.position))
            .and_then(|panes| panes.iter().find(|p| p.is_focused && !p.is_plugin))
            .map(|p| PaneId::Terminal(p.id));

        if new_id == self.state.focused_pane_id
            && (new_id.is_none() || self.state.focused_pane_cwd.is_some())
        {
            return false;
        }

        self.state.focused_pane_id = new_id;

        let new_cwd = match new_id {
            Some(pane_id) => match fetch_cwd(pane_id) {
                Ok(cwd) => {
                    self.pane_cwds.insert(pane_id, cwd.clone());
                    Some(cwd)
                }
                Err(e) => {
                    tracing::debug!("could not get pane cwd: {e}");
                    self.pane_cwds.get(&pane_id).cloned()
                }
            },
            None => None,
        };

        self.set_focused_pane_cwd(new_cwd)
    }

    fn prune_closed_pane_cwds(&mut self, pane_info: &PaneManifest) {
        self.pane_cwds.retain(|pane_id, _| match pane_id {
            PaneId::Terminal(id) => pane_info
                .panes
                .values()
                .flatten()
                .any(|p| !p.is_plugin && p.id == *id),
            PaneId::Plugin(_) => false,
        });
    }

    fn set_focused_pane_cwd(&mut self, new_cwd: Option<PathBuf>) -> bool {
        if new_cwd == self.state.focused_pane_cwd {
            return false;
        }

        self.state.focused_pane_cwd = new_cwd;

        if self.focus_cwd_commands.is_empty() {
            return false;
        }

        self.invalidate_focus_cwd_commands();
        true
    }

    fn invalidate_focus_cwd_commands(&mut self) {
        for name in &self.focus_cwd_commands {
            pipe::invalidate_command_result(&mut self.state, name);
        }
    }

    fn handle_event(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::Mouse(mouse_info) => {
                tracing::Span::current().record("event_type", "Event::Mouse");
                tracing::debug!(mouse = ?mouse_info);

                self.module_config.handle_mouse_action(
                    self.state.clone(),
                    mouse_info,
                    self.widget_map.clone(),
                );
            }
            Event::ModeUpdate(mode_info) => {
                tracing::Span::current().record("event_type", "Event::ModeUpdate");
                tracing::debug!(mode = ?mode_info.mode);
                tracing::debug!(mode = ?mode_info.session_name);

                self.state.mode = mode_info;
                self.state.cache_mask = UpdateEventMask::Mode as u8;

                should_render = true;
            }
            Event::PaneUpdate(pane_info) => {
                tracing::Span::current().record("event_type", "Event::PaneUpdate");
                tracing::debug!(pane_count = ?pane_info.panes.len());

                let frame_config = self.frame_config();
                if !frame_config.is_disabled() {
                    frames::hide_frames_conditionally(
                        &frame_config,
                        &self.state.tabs,
                        &pane_info,
                        &self.state.mode,
                        get_plugin_ids(),
                        false,
                    );
                }

                self.prune_closed_pane_cwds(&pane_info);
                self.state.panes = pane_info;
                self.state.cache_mask = UpdateEventMask::Tab as u8;

                self.update_focused_pane();

                should_render = true;
            }
            Event::CwdChanged(pane_id, cwd, _clients) => {
                tracing::Span::current().record("event_type", "Event::CwdChanged");
                tracing::debug!(pane_id = ?pane_id, cwd = ?cwd);
                self.pane_cwds.insert(pane_id, cwd.clone());

                if self.state.focused_pane_id.is_none() && self.update_focused_pane() {
                    self.state.cache_mask = UpdateEventMask::Command as u8;
                    should_render = true;
                }

                if Some(pane_id) == self.state.focused_pane_id
                    && self.set_focused_pane_cwd(Some(cwd))
                {
                    self.state.cache_mask = UpdateEventMask::Command as u8;
                    should_render = true;
                }
            }
            Event::PermissionRequestResult(result) => {
                tracing::Span::current().record("event_type", "Event::PermissionRequestResult");
                tracing::debug!(result = ?result);
                set_selectable(false);
            }
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                tracing::Span::current().record("event_type", "Event::RunCommandResult");
                tracing::debug!(
                    exit_code = ?exit_code,
                    stdout = ?String::from_utf8(stdout.clone()),
                    stderr = ?String::from_utf8(stderr.clone()),
                    context = ?context
                );

                self.state.cache_mask = UpdateEventMask::Command as u8;

                if let Some(name) = context.get("name") {
                    if self.focus_cwd_commands.iter().any(|n| n == name)
                        && context.get("cwd").map(PathBuf::from) != self.state.focused_pane_cwd
                    {
                        tracing::debug!("discarding stale command result for {name}");
                        return false;
                    }

                    let current_epoch = self.state.command_epochs.get(name).copied().unwrap_or(0);
                    if let Some(res_epoch) =
                        context.get("epoch").and_then(|s| s.parse::<u64>().ok())
                        && res_epoch < current_epoch
                    {
                        tracing::debug!("discarding pre-invalidation command result for {name}");
                        return false;
                    }

                    if let Some(previous) = self.state.command_results.get(name)
                        && let (Some(prev_seq), Some(res_seq)) = (
                            previous
                                .context
                                .get("seq")
                                .and_then(|s| s.parse::<u64>().ok()),
                            context.get("seq").and_then(|s| s.parse::<u64>().ok()),
                        )
                        && res_seq <= prev_seq
                    {
                        tracing::debug!("discarding out-of-order command result for {name}");
                        return false;
                    }

                    let stdout = match String::from_utf8(stdout) {
                        Ok(s) => s,
                        Err(_) => "".to_owned(),
                    };

                    let stderr = match String::from_utf8(stderr) {
                        Ok(s) => s,
                        Err(_) => "".to_owned(),
                    };

                    let key = name.to_owned();
                    let result = CommandResult {
                        exit_code,
                        stdout,
                        stderr,
                        context,
                    };

                    // Repaint when the result changes something the format can
                    // show. Without this the new value sits in state until an
                    // unrelated event or the render timer comes around, which is
                    // visible as latency in any command widget.
                    //
                    // `context` is excluded from the comparison on purpose: it
                    // carries a per-invocation timestamp, and a focus-following
                    // command is re-run on every render until its result lands,
                    // so including it would repaint for identical output.
                    should_render = self.state.command_results.get(&key).is_none_or(|previous| {
                        previous.exit_code != result.exit_code
                            || previous.stdout != result.stdout
                            || previous.stderr != result.stderr
                    });

                    self.state.command_results.insert(key, result);
                }
            }
            Event::SessionUpdate(session_info, _) => {
                tracing::Span::current().record("event_type", "Event::SessionUpdate");

                let frame_config = self.frame_config();
                if !frame_config.is_disabled()
                    && let Some(current_session) =
                        session_info.iter().find(|s| s.is_current_session)
                {
                    frames::hide_frames_conditionally(
                        &frame_config,
                        &current_session.tabs,
                        &current_session.panes,
                        &self.state.mode,
                        get_plugin_ids(),
                        false,
                    );
                }

                self.state.sessions = session_info;
                self.state.cache_mask = UpdateEventMask::Session as u8;

                should_render = true;
            }
            Event::TabUpdate(tab_info) => {
                tracing::Span::current().record("event_type", "Event::TabUpdate");
                tracing::debug!(tab_count = ?tab_info.len());

                self.state.cache_mask = UpdateEventMask::Tab as u8;
                self.state.tabs = tab_info;

                self.update_focused_pane();

                should_render = true;
            }
            Event::Timer(_) => {
                tracing::Span::current().record("event_type", "Event::Timer");
                set_timeout(REFRESH_INTERVAL_SECONDS);
                if !self.focus_cwd_commands.is_empty() && self.state.focused_pane_cwd.is_none() {
                    self.update_focused_pane();
                }
                self.state.cache_mask = 0;

                should_render = true;
            }
            _ => (),
        };
        should_render
    }
}

fn subscribed_events(frame_config: &frames::FrameConfig) -> Vec<EventType> {
    let mut events = vec![
        EventType::Mouse,
        EventType::ModeUpdate,
        EventType::PaneUpdate,
        EventType::PermissionRequestResult,
        EventType::Timer,
        EventType::TabUpdate,
        EventType::RunCommandResult,
        EventType::CwdChanged,
    ];
    if !frame_config.is_disabled() {
        events.push(EventType::SessionUpdate);
    }
    events
}

fn register_widgets(configuration: &BTreeMap<String, String>) -> BTreeMap<String, Arc<dyn Widget>> {
    let mut widget_map = BTreeMap::<String, Arc<dyn Widget>>::new();

    widget_map.insert(
        "command".to_owned(),
        Arc::new(CommandWidget::new(configuration)),
    );
    widget_map.insert(
        "datetime".to_owned(),
        Arc::new(DateTimeWidget::new(configuration)),
    );
    widget_map.insert("pipe".to_owned(), Arc::new(PipeWidget::new(configuration)));
    widget_map.insert(
        "swap_layout".to_owned(),
        Arc::new(SwapLayoutWidget::new(configuration)),
    );
    widget_map.insert("mode".to_owned(), Arc::new(ModeWidget::new(configuration)));
    widget_map.insert(
        "session".to_owned(),
        Arc::new(SessionWidget::new(configuration)),
    );
    widget_map.insert("tabs".to_owned(), Arc::new(TabsWidget::new(configuration)));
    widget_map.insert(
        "notifications".to_owned(),
        Arc::new(NotificationWidget::new(configuration)),
    );

    tracing::debug!("registered widgets: {:?}", widget_map.keys());

    widget_map
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn set_focused_pane_cwd_only_invalidates_on_change() {
        let mut state = State {
            focus_cwd_commands: vec!["command_branch".to_owned()],
            ..State::default()
        };
        state.state.focused_pane_cwd = Some(PathBuf::from("/tmp"));
        state.state.command_results.insert(
            "command_branch".to_owned(),
            CommandResult {
                context: BTreeMap::from([(
                    "timestamp".to_owned(),
                    Local::now()
                        .format(zjstatus::widgets::command::TIMESTAMP_FORMAT)
                        .to_string(),
                )]),
                ..CommandResult::default()
            },
        );

        let original_timestamp =
            state.state.command_results["command_branch"].context["timestamp"].clone();

        assert!(!state.set_focused_pane_cwd(Some(PathBuf::from("/tmp"))));
        assert_eq!(
            state.state.command_results["command_branch"].context["timestamp"],
            original_timestamp
        );

        assert!(state.set_focused_pane_cwd(Some(PathBuf::from("/var"))));
        assert_eq!(state.state.focused_pane_cwd, Some(PathBuf::from("/var")));
        assert_ne!(
            state.state.command_results["command_branch"].context["timestamp"],
            original_timestamp
        );
    }

    #[test]
    fn run_command_result_renders_only_when_output_changes() {
        let mut state = State::default();

        let ctx_t1 = BTreeMap::from([
            ("name".to_owned(), "command_branch".to_owned()),
            ("timestamp".to_owned(), "100".to_owned()),
        ]);
        assert!(state.handle_event(Event::RunCommandResult(
            Some(0),
            b"main\n".to_vec(),
            Vec::new(),
            ctx_t1,
        )));

        // Identical output with a newer timestamp must not schedule a repaint.
        let ctx_t2 = BTreeMap::from([
            ("name".to_owned(), "command_branch".to_owned()),
            ("timestamp".to_owned(), "105".to_owned()),
        ]);
        assert!(!state.handle_event(Event::RunCommandResult(
            Some(0),
            b"main\n".to_vec(),
            Vec::new(),
            ctx_t2.clone(),
        )));

        // Changed stdout, stderr, or exit_code must schedule a repaint.
        assert!(state.handle_event(Event::RunCommandResult(
            Some(0),
            b"feat\n".to_vec(),
            Vec::new(),
            ctx_t2.clone(),
        )));
        assert!(state.handle_event(Event::RunCommandResult(
            Some(1),
            b"feat\n".to_vec(),
            Vec::new(),
            ctx_t2.clone(),
        )));
        assert!(state.handle_event(Event::RunCommandResult(
            Some(1),
            b"feat\n".to_vec(),
            b"err\n".to_vec(),
            ctx_t2,
        )));
    }

    #[test]
    fn pending_events_replay_in_fifo_order_and_propagate_render() {
        let mut state = State::default();
        let pane = PaneId::Terminal(1);

        assert!(!state.update(Event::CwdChanged(pane, PathBuf::from("/first"), Vec::new())));
        assert!(!state.update(Event::CwdChanged(
            pane,
            PathBuf::from("/second"),
            Vec::new(),
        )));
        assert!(!state.update(Event::ModeUpdate(ModeInfo::default())));

        assert!(state.update(Event::PermissionRequestResult(PermissionStatus::Granted)));
        assert_eq!(state.pane_cwds.get(&pane), Some(&PathBuf::from("/second")));
    }

    #[test]
    fn update_focused_pane_retries_none_cwd_and_falls_back_on_timeout() {
        let mut state = State {
            focus_cwd_commands: vec!["command_branch".to_owned()],
            ..State::default()
        };
        state.state.tabs = vec![TabInfo {
            position: 0,
            active: true,
            ..TabInfo::default()
        }];
        state.state.panes.panes.insert(
            0,
            vec![
                PaneInfo {
                    id: 7,
                    is_plugin: false,
                    is_focused: true,
                    ..PaneInfo::default()
                },
                PaneInfo {
                    id: 8,
                    is_plugin: false,
                    is_focused: false,
                    ..PaneInfo::default()
                },
            ],
        );

        // Initial timeout leaves focused_pane_cwd None, allowing retry on the same pane.
        assert!(!state.update_focused_pane_with(|_| Err("GetPaneCwd timeout".to_owned())));
        assert_eq!(state.state.focused_pane_id, Some(PaneId::Terminal(7)));
        assert_eq!(state.state.focused_pane_cwd, None);

        assert!(state.update_focused_pane_with(|_| Ok(PathBuf::from("/repo-a"))));
        assert_eq!(state.state.focused_pane_cwd, Some(PathBuf::from("/repo-a")));

        // Seed pane 8 via asynchronous CwdChanged, then switch focus to pane 8 while
        // GetPaneCwd times out: pane_cwds fallback prevents wedging on None.
        state.handle_event(Event::CwdChanged(
            PaneId::Terminal(8),
            PathBuf::from("/repo-b"),
            Vec::new(),
        ));
        state.state.panes.panes.get_mut(&0).unwrap()[0].is_focused = false;
        state.state.panes.panes.get_mut(&0).unwrap()[1].is_focused = true;

        assert!(state.update_focused_pane_with(|_| Err("GetPaneCwd timeout".to_owned())));
        assert_eq!(state.state.focused_pane_id, Some(PaneId::Terminal(8)));
        assert_eq!(state.state.focused_pane_cwd, Some(PathBuf::from("/repo-b")));

        // Closing pane 7 prunes its cached CWD entry while retaining live pane 8.
        state.state.panes.panes.get_mut(&0).unwrap().remove(0);
        state.prune_closed_pane_cwds(&state.state.panes.clone());
        assert!(!state.pane_cwds.contains_key(&PaneId::Terminal(7)));
        assert_eq!(
            state.pane_cwds.get(&PaneId::Terminal(8)),
            Some(&PathBuf::from("/repo-b"))
        );
    }

    #[test]
    fn tab_update_resolves_focused_pane_from_cached_cwd() {
        let mut state = State {
            focus_cwd_commands: vec!["command_branch".to_owned()],
            ..State::default()
        };
        state.state.panes.panes.insert(
            0,
            vec![PaneInfo {
                id: 3,
                is_plugin: false,
                is_focused: true,
                ..PaneInfo::default()
            }],
        );
        state
            .pane_cwds
            .insert(PaneId::Terminal(3), PathBuf::from("/workspace"));

        assert!(state.handle_event(Event::TabUpdate(vec![TabInfo {
            position: 0,
            active: true,
            ..TabInfo::default()
        }])));
        assert_eq!(state.state.focused_pane_id, Some(PaneId::Terminal(3)));
        assert_eq!(
            state.state.focused_pane_cwd,
            Some(PathBuf::from("/workspace"))
        );
    }

    #[test]
    fn run_command_result_discards_stale_epoch_and_out_of_order_seq() {
        let mut state = State::default();

        // Accept seq 2 in epoch 0.
        state.handle_event(Event::RunCommandResult(
            Some(0),
            b"new\n".to_vec(),
            Vec::new(),
            BTreeMap::from([
                ("name".to_owned(), "command_branch".to_owned()),
                ("epoch".to_owned(), "0".to_owned()),
                ("seq".to_owned(), "2".to_owned()),
            ]),
        ));
        assert_eq!(
            state.state.command_results["command_branch"].stdout,
            "new\n"
        );

        // Out-of-order seq 1 from the same epoch must be rejected.
        state.handle_event(Event::RunCommandResult(
            Some(0),
            b"old\n".to_vec(),
            Vec::new(),
            BTreeMap::from([
                ("name".to_owned(), "command_branch".to_owned()),
                ("epoch".to_owned(), "0".to_owned()),
                ("seq".to_owned(), "1".to_owned()),
            ]),
        ));
        assert_eq!(
            state.state.command_results["command_branch"].stdout,
            "new\n"
        );

        // Bump epoch via rerun invalidation; pre-invalidation seq 3 (epoch 0) is rejected,
        // while post-invalidation seq 4 (epoch 1) is accepted.
        pipe::invalidate_command_result(&mut state.state, "command_branch");
        state.handle_event(Event::RunCommandResult(
            Some(0),
            b"pre_rerun\n".to_vec(),
            Vec::new(),
            BTreeMap::from([
                ("name".to_owned(), "command_branch".to_owned()),
                ("epoch".to_owned(), "0".to_owned()),
                ("seq".to_owned(), "3".to_owned()),
            ]),
        ));
        assert_eq!(
            state.state.command_results["command_branch"].stdout,
            "new\n"
        );

        state.handle_event(Event::RunCommandResult(
            Some(0),
            b"post_rerun\n".to_vec(),
            Vec::new(),
            BTreeMap::from([
                ("name".to_owned(), "command_branch".to_owned()),
                ("epoch".to_owned(), "1".to_owned()),
                ("seq".to_owned(), "4".to_owned()),
            ]),
        ));
        assert_eq!(
            state.state.command_results["command_branch"].stdout,
            "post_rerun\n"
        );
    }

    #[test]
    fn subscribes_to_session_update_only_when_frame_hiding_enabled() {
        let disabled = frames::FrameConfig::new(false, false, false, false, "titles");
        assert!(!subscribed_events(&disabled).contains(&EventType::SessionUpdate));

        let single_pane = frames::FrameConfig::new(true, false, false, false, "titles");
        assert!(subscribed_events(&single_pane).contains(&EventType::SessionUpdate));

        let scroll = frames::FrameConfig::new(false, false, false, true, "full");
        assert!(subscribed_events(&scroll).contains(&EventType::SessionUpdate));
    }
}
