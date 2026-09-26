use zellij_tile::prelude::*;
#[cfg(not(test))]
use zellij_tile::shim::run_command_with_env_variables_and_cwd;

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

const ACTIVE_PROJECT_COLOR_COMMAND: &str = "zjstatus_active_project_color";
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectColorKey {
    cwd: PathBuf,
    tab_name: String,
}

#[derive(Default)]
struct State {
    pending_events: Vec<Event>,
    got_permissions: bool,
    state: ZellijState,
    active_project_color_key: Option<ProjectColorKey>,
    active_project_color_request: Option<ProjectColorKey>,
    userspace_configuration: BTreeMap<String, String>,
    module_config: config::ModuleConfig,
    widget_map: BTreeMap<String, Arc<dyn Widget>>,
    focus_cwd_commands: Vec<String>,
    err: Option<anyhow::Error>,
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

        // we need the ReadApplicationState permission to receive the ModeUpdate and TabUpdate
        // events
        // we need the RunCommands permission to run "cargo test" in a floating window
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
        ]);

        subscribe(&[
            EventType::Mouse,
            EventType::ModeUpdate,
            EventType::PaneUpdate,
            EventType::PermissionRequestResult,
            EventType::Timer,
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::RunCommandResult,
            EventType::CwdChanged,
        ]);
        set_timeout(REFRESH_INTERVAL_SECONDS);

        self.module_config = match ModuleConfig::new(&configuration) {
            Ok(mc) => mc,
            Err(e) => {
                self.err = Some(e);
                return;
            }
        };
        self.widget_map = register_widgets(&configuration);
        self.focus_cwd_commands =
            zjstatus::widgets::command::focus_cwd_command_names(&configuration);
        self.userspace_configuration = configuration;
        self.pending_events = Vec::new();
        self.got_permissions = false;
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
            active_project_color: None,
        };
        self.active_project_color_key = None;
        self.active_project_color_request = None;
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        let mut should_render = false;

        match pipe_message.source {
            PipeSource::Cli(_) => {
                if let Some(input) = pipe_message.payload {
                    should_render = pipe::parse_protocol(&mut self.state, &input);
                }
            }
            PipeSource::Plugin(_) => {
                if let Some(input) = pipe_message.payload {
                    should_render = pipe::parse_protocol(&mut self.state, &input);
                }
            }
            PipeSource::Keybind => {
                if let Some(input) = pipe_message.payload {
                    should_render = pipe::parse_protocol(&mut self.state, &input);
                }
            }
        }

        should_render
    }

    #[tracing::instrument(skip_all, fields(event_type))]
    fn update(&mut self, event: Event) -> bool {
        if let Event::PermissionRequestResult(PermissionStatus::Granted) = event {
            self.got_permissions = true;

            while !self.pending_events.is_empty() {
                tracing::debug!("processing cached event");
                let ev = self.pending_events.pop();

                self.handle_event(ev.unwrap());
            }
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

impl State {
    fn update_focused_pane(&mut self, check_project_color: bool) {
        let active_tab = self.state.tabs.iter().find(|t| t.active);

        let new_id = active_tab
            .and_then(|tab| self.state.panes.panes.get(&tab.position))
            .and_then(|panes| panes.iter().find(|p| p.is_focused && !p.is_plugin))
            .map(|p| PaneId::Terminal(p.id));

        if new_id == self.state.focused_pane_id {
            self.refresh_active_project_color(check_project_color);
            return;
        }

        self.state.focused_pane_id = new_id;

        let new_cwd = match new_id {
            Some(pane_id) => match get_pane_cwd(pane_id) {
                Ok(cwd) => Some(cwd),
                Err(e) => {
                    tracing::debug!("could not get pane cwd: {e}");
                    None
                }
            },
            None => None,
        };

        if !self.set_focused_pane_cwd(new_cwd) {
            self.refresh_active_project_color(check_project_color);
        }
    }

    fn set_focused_pane_cwd(&mut self, new_cwd: Option<PathBuf>) -> bool {
        if new_cwd == self.state.focused_pane_cwd {
            return false;
        }

        self.state.focused_pane_cwd = new_cwd;

        self.invalidate_focus_cwd_commands();
        self.refresh_active_project_color(false);
        true
    }

    // Rechecks on tab updates so external color changes appear without a project change.
    fn refresh_active_project_color(&mut self, check_current_color: bool) {
        let project = self.active_project_color_key();

        if self.active_project_color_key.as_ref() != project.as_ref() {
            self.state.active_project_color = None;
            self.active_project_color_key = None;
        }

        let Some(project) = project else {
            return;
        };

        if self.active_project_color_request.as_ref() == Some(&project)
            || (!check_current_color && self.active_project_color_key.as_ref() == Some(&project))
        {
            return;
        }

        self.active_project_color_request = Some(project.clone());
        self.lookup_active_project_color(&project);
    }

    fn active_project_color_key(&self) -> Option<ProjectColorKey> {
        Some(ProjectColorKey {
            cwd: self.state.focused_pane_cwd.clone()?,
            tab_name: self.active_tab_name()?.to_owned(),
        })
    }

    // Rebuilds a request key from Zellij's command-result context.
    fn project_color_key_from_context(
        context: &BTreeMap<String, String>,
    ) -> Option<ProjectColorKey> {
        Some(ProjectColorKey {
            cwd: PathBuf::from(context.get("cwd")?),
            tab_name: context.get("project_name")?.clone(),
        })
    }

    fn active_tab_name(&self) -> Option<&str> {
        self.state
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.name.as_str())
    }

    // Runs the user-defined lookup with the key needed to reject stale results.
    fn lookup_active_project_color(&self, project: &ProjectColorKey) {
        #[cfg(test)]
        let _ = project;

        #[cfg(not(test))]
        {
            let context = BTreeMap::from([
                ("name".to_owned(), ACTIVE_PROJECT_COLOR_COMMAND.to_owned()),
                ("cwd".to_owned(), project.cwd.to_string_lossy().into_owned()),
                ("project_name".to_owned(), project.tab_name.clone()),
            ]);

            run_command_with_env_variables_and_cwd(
                &[
                    "fish",
                    "-c",
                    "get_project_color $argv[1]",
                    &project.tab_name,
                ],
                BTreeMap::new(),
                project.cwd.clone(),
                context,
            );
        }
    }

    fn invalidate_focus_cwd_commands(&mut self) {
        for name in &self.focus_cwd_commands {
            pipe::invalidate_command_result(&mut self.state, name);
            zjstatus::widgets::command::release_command_lock(&self.state, name);
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

                frames::hide_frames_conditionally(
                    &frames::FrameConfig::new(
                        self.module_config.hide_frame_for_single_pane,
                        self.module_config.hide_frame_except_for_search,
                        self.module_config.hide_frame_except_for_fullscreen,
                        self.module_config.hide_frame_except_for_scroll,
                        &self.module_config.pane_frame_style,
                    ),
                    &self.state.tabs,
                    &pane_info,
                    &self.state.mode,
                    get_plugin_ids(),
                    false,
                );

                self.state.panes = pane_info;
                self.state.cache_mask = UpdateEventMask::Tab as u8;

                self.update_focused_pane(false);

                should_render = true;
            }
            Event::CwdChanged(pane_id, cwd, _clients) => {
                tracing::Span::current().record("event_type", "Event::CwdChanged");
                tracing::debug!(pane_id = ?pane_id, cwd = ?cwd);

                if Some(pane_id) == self.state.focused_pane_id
                    && self.set_focused_pane_cwd(Some(cwd))
                {
                    self.state.cache_mask =
                        UpdateEventMask::Tab as u8 | UpdateEventMask::Command as u8;
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

                if context
                    .get("name")
                    .is_some_and(|name| name == ACTIVE_PROJECT_COLOR_COMMAND)
                {
                    let Some(project) = Self::project_color_key_from_context(&context) else {
                        return false;
                    };
                    if self.active_project_color_request.as_ref() == Some(&project) {
                        self.active_project_color_request = None;
                    }
                    if self.active_project_color_key() != Some(project.clone()) {
                        tracing::debug!("discarding stale active project color result");
                        return false;
                    }

                    let expected_color = match exit_code {
                        Some(1) => Some(anstyle::RgbColor(255, 93, 253).into()),
                        Some(0) => String::from_utf8(stdout).ok().and_then(|stdout| {
                            zjstatus::render::parse_color(
                                stdout.trim(),
                                &self.userspace_configuration,
                            )
                        }),
                        _ => None,
                    };
                    if self.state.active_project_color == expected_color {
                        return false;
                    }

                    self.state.active_project_color = expected_color;
                    self.active_project_color_key =
                        self.state.active_project_color.as_ref().map(|_| project);
                    self.state.cache_mask = UpdateEventMask::Tab as u8;

                    return true;
                }

                self.state.cache_mask = UpdateEventMask::Command as u8;

                if let Some(name) = context.get("name") {
                    if self.focus_cwd_commands.iter().any(|n| n == name)
                        && context.get("cwd").map(PathBuf::from) != self.state.focused_pane_cwd
                    {
                        tracing::debug!("discarding stale command result for {name}");
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

                    self.state.command_results.insert(
                        name.to_owned(),
                        CommandResult {
                            exit_code,
                            stdout,
                            stderr,
                            context,
                        },
                    );
                }
            }
            Event::SessionUpdate(session_info, _) => {
                tracing::Span::current().record("event_type", "Event::SessionUpdate");

                let current_session = session_info.iter().find(|s| s.is_current_session);

                if let Some(current_session) = current_session {
                    frames::hide_frames_conditionally(
                        &frames::FrameConfig::new(
                            self.module_config.hide_frame_for_single_pane,
                            self.module_config.hide_frame_except_for_search,
                            self.module_config.hide_frame_except_for_fullscreen,
                            self.module_config.hide_frame_except_for_scroll,
                            &self.module_config.pane_frame_style,
                        ),
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
                self.update_focused_pane(true);

                should_render = true;
            }
            Event::Timer(_) => {
                tracing::Span::current().record("event_type", "Event::Timer");
                set_timeout(REFRESH_INTERVAL_SECONDS);
                self.state.cache_mask = 0;

                should_render = true;
            }
            _ => (),
        };
        should_render
    }
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

    fn project_color_state(project_name: &str) -> State {
        let mut state = State::default();
        state.state.focused_pane_cwd = Some(PathBuf::from("/project"));
        state.state.tabs = vec![TabInfo {
            active: true,
            name: project_name.to_owned(),
            ..TabInfo::default()
        }];
        state
    }

    fn project_color_result(
        cwd: &str,
        project_name: &str,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
    ) -> Event {
        Event::RunCommandResult(
            exit_code,
            stdout,
            Vec::new(),
            BTreeMap::from([
                ("name".to_owned(), ACTIVE_PROJECT_COLOR_COMMAND.to_owned()),
                ("cwd".to_owned(), cwd.to_owned()),
                ("project_name".to_owned(), project_name.to_owned()),
            ]),
        )
    }

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
    fn matching_project_color_result_updates_active_project_color() {
        let mut state = project_color_state("project");

        assert!(state.handle_event(project_color_result(
            "/project",
            "project",
            Some(0),
            b" #c77dff\n".to_vec(),
        )));
        assert_eq!(
            state.state.active_project_color,
            Some(anstyle::RgbColor(199, 125, 255).into())
        );
        assert_eq!(state.state.cache_mask, UpdateEventMask::Tab as u8);
    }

    #[test]
    fn refresh_project_color_retains_only_the_active_tab_color() {
        let mut state = project_color_state("project");
        let color = anstyle::RgbColor(199, 125, 255).into();

        assert!(state.handle_event(project_color_result(
            "/project",
            "project",
            Some(0),
            b"#c77dff".to_vec(),
        )));
        state.refresh_active_project_color(false);
        assert_eq!(state.state.active_project_color, Some(color));

        state.state.tabs[0].name = "other-project".to_owned();
        state.refresh_active_project_color(false);
        assert_eq!(state.state.active_project_color, None);
    }

    #[test]
    fn refresh_project_color_deduplicates_matching_lookup() {
        let mut state = project_color_state("project");

        state.refresh_active_project_color(false);
        let request = state.active_project_color_request.clone();
        state.refresh_active_project_color(false);

        assert_eq!(state.active_project_color_request, request);
    }

    #[test]
    fn tab_update_rechecks_the_active_project_color() {
        let mut state = project_color_state("project");

        assert!(state.handle_event(project_color_result(
            "/project",
            "project",
            Some(0),
            b"#c77dff".to_vec(),
        )));
        let tabs = state.state.tabs.clone();
        assert!(state.handle_event(Event::TabUpdate(tabs)));

        assert_eq!(
            state.active_project_color_request,
            Some(ProjectColorKey {
                cwd: PathBuf::from("/project"),
                tab_name: "project".to_owned(),
            })
        );
    }

    #[test]
    fn unchanged_project_color_result_does_not_render() {
        let mut state = project_color_state("project");
        state.state.active_project_color = Some(anstyle::RgbColor(199, 125, 255).into());
        state.active_project_color_key = state.active_project_color_key();
        state.state.cache_mask = 0;

        assert!(!state.handle_event(project_color_result(
            "/project",
            "project",
            Some(0),
            b"#c77dff".to_vec(),
        )));
        assert_eq!(state.state.cache_mask, 0);
    }

    #[test]
    fn missing_project_exit_code_uses_default_normal_color() {
        let mut state = project_color_state("project");

        assert!(state.handle_event(project_color_result(
            "/project",
            "project",
            Some(1),
            Vec::new(),
        )));
        assert_eq!(
            state.state.active_project_color,
            Some(anstyle::RgbColor(255, 93, 253).into())
        );
    }

    #[test]
    fn stale_project_color_result_is_ignored() {
        let mut state = project_color_state("current-project");
        state.state.active_project_color = Some(anstyle::RgbColor(199, 125, 255).into());

        assert!(!state.handle_event(project_color_result(
            "/project",
            "old-project",
            Some(0),
            b"#00ff00".to_vec(),
        )));
        assert_eq!(
            state.state.active_project_color,
            Some(anstyle::RgbColor(199, 125, 255).into())
        );
    }

    #[test]
    fn failed_or_invalid_project_color_result_clears_active_project_color() {
        let mut state = project_color_state("project");

        for event in [
            project_color_result("/project", "project", Some(2), b"#c77dff".to_vec()),
            project_color_result("/project", "project", Some(0), b"not-a-color".to_vec()),
        ] {
            state.state.active_project_color = Some(anstyle::RgbColor(199, 125, 255).into());

            assert!(state.handle_event(event));
            assert_eq!(state.state.active_project_color, None);
            assert_eq!(state.state.cache_mask, UpdateEventMask::Tab as u8);
        }
    }
}
