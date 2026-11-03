use kdl::{KdlDocument, KdlError};
use lazy_static::lazy_static;
use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    ops::Sub,
    path::{Path, PathBuf},
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
};

use chrono::{DateTime, Duration, Local};
use regex::Regex;
#[cfg(all(not(feature = "bench"), not(test)))]
use zellij_tile::shim::{run_command, run_command_with_env_variables_and_cwd};

use crate::render::{FormattedPart, formatted_parts_from_string_cached};

use crate::{config::ZellijState, widgets::widget::Widget};

pub const TIMESTAMP_FORMAT: &str = "%s";
const IN_FLIGHT_TIMEOUT_SECS: i64 = 30;

lazy_static! {
    static ref COMMAND_REGEX: Regex = Regex::new("_[a-zA-Z0-9]+$").unwrap();
}

const FOCUSED_PANE_CWD: &str = "{focused_pane_cwd}";

#[derive(Clone, Debug, PartialEq)]
enum RenderMode {
    Static,
    Dynamic,
    Raw,
}

#[derive(Clone, Debug)]
struct CommandConfig {
    command: String,
    format: Vec<FormattedPart>,
    env: Option<BTreeMap<String, String>>,
    cwd: Option<PathBuf>,
    follow_focus_cwd: bool,
    interval: i64,
    render_mode: RenderMode,
    click_action: String,
    hide_on_empty_stdout: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub context: BTreeMap<String, String>,
}

#[derive(Default, Debug)]
struct InFlightTracker {
    next_seq: AtomicU64,
    slots: BTreeMap<String, InFlightSlot>,
}

impl InFlightTracker {
    fn from_config(config: &BTreeMap<String, CommandConfig>) -> Self {
        let slots = config
            .keys()
            .map(|name| (name.clone(), InFlightSlot::default()))
            .collect();
        Self {
            next_seq: AtomicU64::new(0),
            slots,
        }
    }
}

#[derive(Default, Debug)]
struct InFlightSlot {
    seq: AtomicU64,
    epoch: AtomicU64,
    cwd_hash: AtomicU64,
    spawned_at_secs: AtomicI64,
}

fn hash_cwd(cwd: Option<&Path>) -> u64 {
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    hasher.finish()
}

pub struct CommandWidget {
    config: BTreeMap<String, CommandConfig>,
    zj_conf: BTreeMap<String, String>,
    in_flight: InFlightTracker,
}

impl CommandWidget {
    pub fn new(config: &BTreeMap<String, String>) -> Self {
        let parsed = parse_config(config);
        let in_flight = InFlightTracker::from_config(&parsed);
        Self {
            config: parsed,
            zj_conf: config.clone(),
            in_flight,
        }
    }
}

impl Widget for CommandWidget {
    fn process(&self, name: &str, state: &ZellijState) -> String {
        let command_config = match self.config.get(name) {
            Some(cc) => cc,
            None => {
                return "".to_owned();
            }
        };

        run_command_if_needed(command_config, name, state, &self.in_flight);

        let command_result = match state.command_results.get(name) {
            Some(cr) => cr,
            None => {
                return "".to_owned();
            }
        };

        if command_config.hide_on_empty_stdout && command_result.stdout.is_empty() {
            return "".to_owned();
        }

        let content = command_config
            .format
            .iter()
            .map(|f| {
                let mut content = f.content.clone();

                if content.contains("{exit_code}") {
                    content = content.replace(
                        "{exit_code}",
                        format!("{}", command_result.exit_code.unwrap_or(-1)).as_str(),
                    );
                }

                if content.contains("{stdout}") {
                    content = content.replace(
                        "{stdout}",
                        command_result
                            .stdout
                            .strip_suffix('\n')
                            .unwrap_or(&command_result.stdout),
                    );
                }

                if content.contains("{stderr}") {
                    content = content.replace(
                        "{stderr}",
                        command_result
                            .stderr
                            .strip_suffix('\n')
                            .unwrap_or(&command_result.stderr),
                    );
                }

                (f, content)
            })
            .fold("".to_owned(), |acc, (f, content)| {
                if command_config.render_mode == RenderMode::Static {
                    return format!("{acc}{}", f.format_string(&content));
                }

                format!("{acc}{}", content)
            });

        match command_config.render_mode {
            RenderMode::Static => content,
            RenderMode::Dynamic => render_dynamic_formatted_content(&content, &self.zj_conf),
            RenderMode::Raw => command_result
                .stdout
                .strip_suffix('\n')
                .unwrap_or(&command_result.stdout)
                .to_owned(),
        }
    }

    fn process_click(&self, name: &str, _state: &ZellijState, _pos: usize) {
        let command_config = match self.config.get(name) {
            Some(cc) => cc,
            None => {
                return;
            }
        };

        if command_config.click_action.is_empty() {
            return;
        }

        let command = commandline_parser(&command_config.click_action);
        let context: BTreeMap<String, String> = BTreeMap::new();

        tracing::debug!("Running command {:?} {:?}", command, context);

        #[cfg(all(not(feature = "bench"), not(test)))]
        run_command(
            &command.iter().map(|x| x.as_str()).collect::<Vec<&str>>(),
            context,
        );
    }
}

fn render_dynamic_formatted_content(content: &str, config: &BTreeMap<String, String>) -> String {
    formatted_parts_from_string_cached(content, config)
        .iter()
        .map(|fp| fp.format_string(&fp.content))
        .collect::<Vec<String>>()
        .join("")
}

#[tracing::instrument(skip(command_config, state, tracker))]
fn run_command_if_needed(
    command_config: &CommandConfig,
    name: &str,
    state: &ZellijState,
    tracker: &InFlightTracker,
) -> bool {
    let current_epoch = state.command_epochs.get(name).copied().unwrap_or(0);

    if command_config.interval == 0
        && let Some(existing) = state.command_results.get(name)
    {
        let result_epoch = existing
            .context
            .get("epoch")
            .and_then(|e| e.parse::<u64>().ok())
            .unwrap_or(0);
        if result_epoch == current_epoch {
            return false;
        }
    }

    let ts = Local::now();
    let last_run = get_timestamp_from_event_or_default(name, state);

    if ts.timestamp() - last_run.timestamp() >= command_config.interval {
        let cwd = if command_config.follow_focus_cwd {
            match &state.focused_pane_cwd {
                Some(cwd) => Some(cwd.clone()),
                None => return false,
            }
        } else {
            command_config.cwd.clone()
        };

        let completed_seq = state
            .command_results
            .get(name)
            .and_then(|r| r.context.get("seq"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let cwd_hash = hash_cwd(cwd.as_deref());

        if let Some(slot) = tracker.slots.get(name) {
            let in_flight_seq = slot.seq.load(Ordering::Relaxed);
            if in_flight_seq > completed_seq
                && slot.epoch.load(Ordering::Relaxed) == current_epoch
                && slot.cwd_hash.load(Ordering::Relaxed) == cwd_hash
                && (0..IN_FLIGHT_TIMEOUT_SECS)
                    .contains(&(ts.timestamp() - slot.spawned_at_secs.load(Ordering::Relaxed)))
            {
                return false;
            }
        }

        let seq = tracker.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(slot) = tracker.slots.get(name) {
            slot.seq.store(seq, Ordering::Relaxed);
            slot.epoch.store(current_epoch, Ordering::Relaxed);
            slot.cwd_hash.store(cwd_hash, Ordering::Relaxed);
            slot.spawned_at_secs
                .store(ts.timestamp(), Ordering::Relaxed);
        }

        let mut context = BTreeMap::new();
        context.insert("name".to_owned(), name.to_owned());
        context.insert(
            "timestamp".to_owned(),
            ts.format(TIMESTAMP_FORMAT).to_string(),
        );
        context.insert("epoch".to_owned(), current_epoch.to_string());
        context.insert("seq".to_owned(), seq.to_string());

        if let Some(cwd) = &cwd {
            context.insert("cwd".to_owned(), cwd.to_string_lossy().into_owned());
        }

        #[allow(unused_variables)]
        let command = commandline_parser(&command_config.command);
        tracing::debug!("Running command: {:?}", command);

        if command_config.env.is_some() || cwd.is_some() {
            #[cfg(all(not(feature = "bench"), not(test)))]
            run_command_with_env_variables_and_cwd(
                &command.iter().map(|x| x.as_str()).collect::<Vec<&str>>(),
                command_config.env.clone().unwrap_or_default(),
                cwd.unwrap_or_default(),
                context,
            );

            return true;
        }

        #[cfg(all(not(feature = "bench"), not(test)))]
        run_command(
            &command.iter().map(|x| x.as_str()).collect::<Vec<&str>>(),
            context,
        );

        return true;
    }

    false
}

pub fn focus_cwd_command_names(zj_conf: &BTreeMap<String, String>) -> Vec<String> {
    parse_config(zj_conf)
        .into_iter()
        .filter(|(_, conf)| conf.follow_focus_cwd)
        .map(|(name, _)| name)
        .collect()
}

fn parse_config(zj_conf: &BTreeMap<String, String>) -> BTreeMap<String, CommandConfig> {
    let mut keys: Vec<String> = zj_conf
        .keys()
        .filter(|k| k.starts_with("command_"))
        .cloned()
        .collect();
    keys.sort();

    let mut config: BTreeMap<String, CommandConfig> = BTreeMap::new();

    for key in keys {
        let command_name = COMMAND_REGEX.replace(&key, "").to_string();

        let mut command_conf = CommandConfig {
            command: "".to_owned(),
            format: Vec::new(),
            cwd: None,
            follow_focus_cwd: false,
            env: None,
            interval: 1,
            render_mode: RenderMode::Static,
            click_action: "".to_owned(),
            hide_on_empty_stdout: false,
        };

        if let Some(existing_conf) = config.get(command_name.as_str()) {
            command_conf = existing_conf.clone();
        }

        if key.ends_with("command") {
            command_conf
                .command
                .clone_from(&zj_conf.get(&key).unwrap().to_owned());
        }

        if key.ends_with("clickaction") {
            command_conf
                .click_action
                .clone_from(&zj_conf.get(&key).unwrap().to_owned());
        }

        if key.ends_with("env") {
            let doc: Result<KdlDocument, KdlError> = zj_conf.get(&key).unwrap().parse();

            if let Ok(doc) = doc {
                command_conf.env = Some(get_env_vars(doc));
            }
        }

        if key.ends_with("cwd") {
            let value = zj_conf.get(&key).unwrap().to_owned();

            if value == FOCUSED_PANE_CWD {
                command_conf.follow_focus_cwd = true;
                command_conf.cwd = None;
            } else {
                let mut cwd = PathBuf::new();
                cwd.push(value);

                command_conf.cwd = Some(cwd);
                command_conf.follow_focus_cwd = false;
            }
        }

        if key.ends_with("format") {
            command_conf.format =
                FormattedPart::multiple_from_format_string(zj_conf.get(&key).unwrap(), zj_conf);
        }

        if key.ends_with("interval") {
            command_conf.interval = zj_conf.get(&key).unwrap().parse::<i64>().unwrap_or(1);
        }

        if key.ends_with("rendermode") {
            command_conf.render_mode = match zj_conf.get(&key) {
                Some(mode) => match mode.as_str() {
                    "static" => RenderMode::Static,
                    "dynamic" => RenderMode::Dynamic,
                    "raw" => RenderMode::Raw,
                    _ => RenderMode::Static,
                },
                None => RenderMode::Static,
            };
        }

        if key.ends_with("hideonemptystdout") {
            command_conf.hide_on_empty_stdout = match zj_conf.get(&key) {
                Some(val) => val == "true",
                None => false,
            };
        }

        config.insert(command_name, command_conf);
    }

    config
}

fn get_env_vars(doc: KdlDocument) -> BTreeMap<String, String> {
    let mut output = BTreeMap::new();

    for n in doc.nodes() {
        let children = n.entries();
        if children.len() != 1 {
            continue;
        }

        let value = match children.first().unwrap().value().as_string() {
            Some(value) => value,
            None => continue,
        };

        output.insert(n.name().value().to_string(), value.to_string());
    }

    output
}

fn get_timestamp_from_event_or_default(name: &str, state: &ZellijState) -> DateTime<Local> {
    let Some(command_result) = state.command_results.get(name) else {
        return Sub::<Duration>::sub(Local::now(), Duration::try_days(1).unwrap());
    };

    let Some(ts_context) = command_result.context.get("timestamp") else {
        return Sub::<Duration>::sub(Local::now(), Duration::try_days(1).unwrap());
    };

    match DateTime::parse_from_str(ts_context, TIMESTAMP_FORMAT) {
        Ok(ts) => ts.into(),
        Err(_) => Sub::<Duration>::sub(Local::now(), Duration::try_days(1).unwrap()),
    }
}

fn commandline_parser(input: &str) -> Vec<String> {
    let mut output: Vec<String> = Vec::new();

    let special_chars = ['"', '\''];

    let mut found_special_char = '\0';
    let mut buffer = "".to_owned();
    let mut is_escaped = false;
    let mut is_in_group = false;

    for character in input.chars() {
        if is_escaped {
            is_escaped = false;
            buffer = format!("{}\\{}", buffer.to_owned(), character);
            continue;
        }

        if character == '\\' {
            is_escaped = true;
            continue;
        }

        if found_special_char == character && is_in_group {
            is_in_group = false;
            found_special_char = '\0';
            output.push(buffer.clone());
            "".clone_into(&mut buffer);
            continue;
        }

        if special_chars.contains(&character) && !is_in_group {
            is_in_group = true;
            found_special_char = character;
            continue;
        }

        if character == ' ' && !is_in_group {
            output.push(buffer.clone());
            "".clone_into(&mut buffer);
            continue;
        }

        buffer = format!("{}{}", buffer, character);
    }

    if !buffer.is_empty() {
        output.push(buffer.clone());
    }

    output
}

#[cfg(test)]
mod test {
    use super::*;
    use rstest::rstest;

    #[test]
    pub fn test_focused_pane_cwd_parsing() {
        let conf = BTreeMap::from([
            ("command_branch_command".to_owned(), "git branch".to_owned()),
            (
                "command_branch_cwd".to_owned(),
                "{focused_pane_cwd}".to_owned(),
            ),
            ("command_static_command".to_owned(), "ls".to_owned()),
            ("command_static_cwd".to_owned(), "/tmp".to_owned()),
        ]);

        let config = parse_config(&conf);
        assert!(config.get("command_branch").unwrap().follow_focus_cwd);
        assert_eq!(config.get("command_branch").unwrap().cwd, None);
        assert!(!config.get("command_static").unwrap().follow_focus_cwd);
        assert_eq!(
            config.get("command_static").unwrap().cwd,
            Some(PathBuf::from("/tmp"))
        );

        assert_eq!(focus_cwd_command_names(&conf), vec!["command_branch"]);
    }

    #[test]
    pub fn test_commandline_parser() {
        let input = "pwd";
        let result = commandline_parser(input);
        let expected = Vec::from(["pwd"]);
        assert_eq!(result, expected);

        let input = "bash -c \"pwd | base64 -c \\\"bla\\\"\"";
        let result = commandline_parser(input);
        let expected = Vec::from(["bash", "-c", "pwd | base64 -c \\\"bla\\\""]);
        assert_eq!(result, expected);

        let input = "bash -c \"pwd | base64 -c 'bla' | xxd\"";
        let result = commandline_parser(input);
        let expected = Vec::from(["bash", "-c", "pwd | base64 -c 'bla' | xxd"]);
        assert_eq!(result, expected);
    }

    #[test]
    pub fn test_in_flight_deduplicates_reruns_and_unblocks_on_invalidation_or_completion() {
        let mut state = ZellijState {
            focused_pane_cwd: Some(PathBuf::from("/repo-a")),
            ..ZellijState::default()
        };
        let command_config = CommandConfig {
            command: "git status".to_owned(),
            format: Vec::new(),
            env: None,
            cwd: None,
            follow_focus_cwd: true,
            interval: 5,
            render_mode: RenderMode::Static,
            click_action: "".to_owned(),
            hide_on_empty_stdout: false,
        };
        let tracker = InFlightTracker {
            next_seq: AtomicU64::new(0),
            slots: BTreeMap::from([("command_git_branch".to_owned(), InFlightSlot::default())]),
        };

        // First spawn succeeds; duplicate renders while in flight are suppressed.
        assert!(run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));
        assert!(!run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));

        // Explicit invalidation (even before any result lands) unblocks a fresh run.
        crate::pipe::invalidate_command_result(&mut state, "command_git_branch");
        assert!(run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));
        assert!(!run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));

        // Recording a completed result with seq == 2 and expired timestamp allows the next
        // scheduled interval run, while deduplicating subsequent renders until seq == 3 lands.
        state.command_results.insert(
            "command_git_branch".to_owned(),
            CommandResult {
                context: BTreeMap::from([
                    ("timestamp".to_owned(), "0".to_owned()),
                    ("epoch".to_owned(), "1".to_owned()),
                    ("seq".to_owned(), "2".to_owned()),
                ]),
                ..CommandResult::default()
            },
        );
        assert!(run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));
        assert!(!run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));

        // Moving to a different focused CWD unblocks immediately without waiting for seq == 3.
        state.focused_pane_cwd = Some(PathBuf::from("/repo-b"));
        assert!(run_command_if_needed(
            &command_config,
            "command_git_branch",
            &state,
            &tracker
        ));
    }

    #[rstest]
    // no result, interval 1 second
    #[case(1, &ZellijState{ plugin_uuid: "test_command_result_1".to_owned(), ..Default::default() }, true)]
    // only run once without a result
    #[case(0, &ZellijState::default(), true)]
    // do not run with run once and result
    #[case(0, &ZellijState {
        command_results: BTreeMap::from([(
            "test".to_owned(),
            CommandResult::default(),
        )]),
        ..ZellijState::default()
    }, false)]
    // run if interval is exceeded
    #[case(1, &ZellijState {
        command_results: BTreeMap::from([(
            "test".to_owned(),
            CommandResult{
                context: BTreeMap::from([("timestamp".to_owned(), "0".to_owned())]),
                ..CommandResult::default()
            }
        )]),
        ..ZellijState::default()
    }, true)]
    // do not run if interval is not exceeded
    #[case(1, &ZellijState {
        command_results: BTreeMap::from([(
            "test".to_owned(),
            CommandResult{
                context: BTreeMap::from([("timestamp".to_owned(), Local::now().format(TIMESTAMP_FORMAT).to_string())]),
                ..CommandResult::default()
            }
        )]),
        ..ZellijState::default()
    }, false)]
    pub fn test_run_command_if_needed(
        #[case] interval: i64,
        #[case] state: &ZellijState,
        #[case] expected: bool,
    ) {
        let tracker = InFlightTracker {
            next_seq: AtomicU64::new(0),
            slots: BTreeMap::from([("test".to_owned(), InFlightSlot::default())]),
        };
        let res = run_command_if_needed(
            &CommandConfig {
                command: "echo test".to_owned(),
                format: Vec::new(),
                env: None,
                cwd: None,
                follow_focus_cwd: false,
                interval,
                render_mode: RenderMode::Static,
                click_action: "".to_owned(),
                hide_on_empty_stdout: false,
            },
            "test",
            state,
            &tracker,
        );
        assert_eq!(res, expected);
    }
}
