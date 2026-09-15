//! The TUI's own settings: which Claude model and `--effort` each feature runs with — the
//! summary writer and the question session. Edited in the settings modal
//! (`,` in the TUI), kept in `<data dir>/settings.json` per user (never inside the
//! repository, never in the engine's `meet.json`), and applied to the next call of that
//! feature the moment a value changes.
//!
//! Where a value comes from, first match wins: the launch's CLI flag (`--suggest-model` …,
//! for this run only), the settings file, then the built-in default. `"default"` means
//! "claude's own" — no `--model` / `--effort` is passed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "settings.json";
/// The value that passes no flag: claude's own model or effort.
pub const DEFAULT: &str = "default";

/// The Claude models offered in the modal: claude's aliases first, then the ids. Any
/// other value (typed into the file, or passed on the command line) is kept and shown
/// as it is.
pub const MODELS: &[&str] = &[
    DEFAULT,
    "haiku",
    "sonnet",
    "opus",
    "fable",
    "claude-haiku-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-fable-5",
    "claude-fable-5-1",
];

/// The `--effort` levels claude takes.
pub const EFFORTS: &[&str] = &[DEFAULT, "low", "medium", "high", "xhigh", "max"];

/// A feature that calls Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// The summary writer that runs when the recording stops.
    Suggest,
    /// The question session (`a`, `meet ask`).
    Ask,
}

impl Feature {
    pub const ALL: [Feature; 2] = [Feature::Suggest, Feature::Ask];

    /// The tab / section title.
    pub fn title(self) -> &'static str {
        match self {
            Feature::Suggest => "Summary",
            Feature::Ask => "Ask",
        }
    }

    /// One line on what the feature is, for the modal (fits its width).
    pub fn blurb(self) -> &'static str {
        match self {
            Feature::Suggest => {
                "the meeting summary: one call over the whole transcript when the recording stops"
            }
            Feature::Ask => "the question session: Claude Code in the right pane (a) and `meet ask`",
        }
    }

    /// When a changed value takes effect.
    pub fn applies_to(self) -> &'static str {
        match self {
            Feature::Suggest => "the next summary",
            Feature::Ask => "the next question session",
        }
    }

    /// The JSON key in the settings file and the CLI flag prefix.
    pub fn key(self) -> &'static str {
        match self {
            Feature::Suggest => "suggest",
            Feature::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Model,
    Effort,
}

impl Field {
    pub const ALL: [Field; 2] = [Field::Model, Field::Effort];

    pub fn label(self) -> &'static str {
        match self {
            Field::Model => "model",
            Field::Effort => "effort",
        }
    }

    pub fn choices(self) -> &'static [&'static str] {
        match self {
            Field::Model => MODELS,
            Field::Effort => EFFORTS,
        }
    }

    /// What the field means, for the modal's hint line.
    pub fn hint(self) -> &'static str {
        match self {
            Field::Model => {
                "an alias (haiku, sonnet, opus, fable) or a full id; default passes no --model"
            }
            Field::Effort => {
                "low is fastest and cheapest, max thinks longest; default passes no --effort"
            }
        }
    }
}

/// The settings in the order the modal lists them: each feature's model, then its effort.
pub const ROWS: [(Feature, Field); 4] = [
    (Feature::Suggest, Field::Model),
    (Feature::Suggest, Field::Effort),
    (Feature::Ask, Field::Model),
    (Feature::Ask, Field::Effort),
];

/// The CLI flag that overrides a setting for one launch: `--suggest-model`, `--ask-effort`.
pub fn flag_name(feature: Feature, field: Field) -> String {
    format!("--{}-{}", feature.key(), field.label())
}

/// What the modal draws, top to bottom: a header per feature, its rows, a blank between
/// features. One map for the renderer and the cursor, so they cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Blank,
    Header(Feature),
    /// An index into [`ROWS`].
    Setting(usize),
}

pub fn rows() -> Vec<Row> {
    let mut out = Vec::new();
    let mut current: Option<Feature> = None;
    for (i, (feature, _)) in ROWS.iter().enumerate() {
        if current != Some(*feature) {
            if !out.is_empty() {
                out.push(Row::Blank);
            }
            out.push(Row::Header(*feature));
            current = Some(*feature);
        }
        out.push(Row::Setting(i));
    }
    out
}

/// The first row of a feature in [`ROWS`], for the `1`–`2` jump.
pub fn first_row_of(feature: Feature) -> usize {
    ROWS.iter().position(|(f, _)| *f == feature).unwrap_or(0)
}

/// The values passed on the command line this launch (`--suggest-model haiku`): they win
/// over the file until `meet` restarts, and the modal says so beside the row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides(HashMap<(Feature, Field), String>);

impl Overrides {
    /// `None` and blank mean "not passed"; `"default"` is an explicit override to claude's own.
    pub fn set(&mut self, feature: Feature, field: Field, value: Option<String>) {
        match value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            Some(v) => {
                self.0.insert((feature, field), v);
            }
            None => {
                self.0.remove(&(feature, field));
            }
        }
    }

    pub fn get(&self, feature: Feature, field: Field) -> Option<&str> {
        self.0.get(&(feature, field)).map(String::as_str)
    }
}

/// One feature's pair of values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub model: String,
    pub effort: String,
}

impl Pair {
    fn new(model: &str, effort: &str) -> Self {
        Self {
            model: model.into(),
            effort: effort.into(),
        }
    }

    pub fn get(&self, field: Field) -> &str {
        match field {
            Field::Model => &self.model,
            Field::Effort => &self.effort,
        }
    }

    fn get_mut(&mut self, field: Field) -> &mut String {
        match field {
            Field::Model => &mut self.model,
            Field::Effort => &mut self.effort,
        }
    }
}

/// Every feature's model and effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub suggest: Pair,
    pub ask: Pair,
}

impl Default for Settings {
    /// What `meet` ran with before there was a settings file.
    fn default() -> Self {
        Self {
            suggest: Pair::new("sonnet", DEFAULT),
            ask: Pair::new("sonnet", DEFAULT),
        }
    }
}

/// `"default"`, blank and whitespace all mean claude's own; anything else is kept trimmed.
pub fn normalize(value: &str) -> String {
    let v = value.trim();
    if v.is_empty() || v == DEFAULT {
        DEFAULT.to_string()
    } else {
        v.to_string()
    }
}

/// The flag value to pass: `None` for claude's own.
pub fn as_flag(value: &str) -> Option<String> {
    let v = normalize(value);
    (v != DEFAULT).then_some(v)
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::paths::data_dir().join(FILE)
    }

    /// The path for the modal's footer, with the home directory as `~`.
    pub fn path_display() -> String {
        let path = Self::path();
        if let Some(home) = std::env::var_os("HOME") {
            if let Ok(rest) = path.strip_prefix(&home) {
                return format!("~/{}", rest.display());
            }
        }
        path.display().to_string()
    }

    pub fn pair(&self, feature: Feature) -> &Pair {
        match feature {
            Feature::Suggest => &self.suggest,
            Feature::Ask => &self.ask,
        }
    }

    fn pair_mut(&mut self, feature: Feature) -> &mut Pair {
        match feature {
            Feature::Suggest => &mut self.suggest,
            Feature::Ask => &mut self.ask,
        }
    }

    pub fn get(&self, feature: Feature, field: Field) -> &str {
        self.pair(feature).get(field)
    }

    pub fn set(&mut self, feature: Feature, field: Field, value: &str) {
        *self.pair_mut(feature).get_mut(field) = normalize(value);
    }

    /// The flag value for a feature's field: `None` for claude's own.
    pub fn flag(&self, feature: Feature, field: Field) -> Option<String> {
        as_flag(self.get(feature, field))
    }

    /// Step a value through its choices: `+1` the next, `-1` the previous, wrapping. A
    /// value outside the list steps to the first choice.
    pub fn cycle(&mut self, feature: Feature, field: Field, delta: isize) {
        let choices = field.choices();
        let current = self.get(feature, field).to_string();
        let next = match choices.iter().position(|c| *c == current) {
            Some(i) => {
                let n = choices.len() as isize;
                choices[((i as isize + delta).rem_euclid(n)) as usize]
            }
            None => choices[0],
        };
        self.set(feature, field, next);
    }

    /// Every value back to the built-in default.
    pub fn reset(&mut self) {
        *self = Settings::default();
    }

    /// Read the file; a missing file is the defaults, a broken one is an error naming it.
    /// Unknown keys are ignored, missing ones take their default.
    pub fn load_from(path: &Path) -> Result<Settings> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut s: Settings = serde_json::from_str(&text)
            .with_context(|| format!("could not parse {}", path.display()))?;
        for f in Feature::ALL {
            for field in Field::ALL {
                let v = normalize(s.get(f, field));
                s.set(f, field, &v);
            }
        }
        Ok(s)
    }

    pub fn load() -> Result<Settings> {
        Self::load_from(&Self::path())
    }

    /// Write the file whole (pretty JSON), through a temp file so a crash mid-write never
    /// leaves a half file behind.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("move {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }
}

/// What a feature actually runs with this launch: the CLI flag when one was passed, else
/// the setting; `None` is claude's own.
pub fn effective(
    settings: &Settings,
    feature: Feature,
    field: Field,
    cli: Option<&str>,
) -> Option<String> {
    match cli {
        Some(c) => as_flag(c),
        None => settings.flag(feature, field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_what_meet_ran_with_and_default_means_no_flag() {
        let s = Settings::default();
        assert_eq!(s.flag(Feature::Suggest, Field::Model).as_deref(), Some("sonnet"));
        assert_eq!(s.flag(Feature::Suggest, Field::Effort), None);
        assert_eq!(s.flag(Feature::Ask, Field::Model).as_deref(), Some("sonnet"));
        assert_eq!(s.get(Feature::Ask, Field::Effort), DEFAULT);
        assert_eq!(normalize("  "), DEFAULT);
        assert_eq!(normalize(" opus "), "opus");
        assert_eq!(as_flag("default"), None);
    }

    #[test]
    fn cycling_wraps_and_an_unknown_value_steps_to_the_first_choice() {
        let mut s = Settings::default();
        s.cycle(Feature::Suggest, Field::Effort, 1);
        assert_eq!(s.get(Feature::Suggest, Field::Effort), "low");
        s.cycle(Feature::Suggest, Field::Effort, 1);
        assert_eq!(s.get(Feature::Suggest, Field::Effort), "medium");
        s.cycle(Feature::Suggest, Field::Effort, -2);
        assert_eq!(s.get(Feature::Suggest, Field::Effort), DEFAULT);
        s.cycle(Feature::Suggest, Field::Effort, -1);
        assert_eq!(s.get(Feature::Suggest, Field::Effort), "max", "wraps backwards");
        s.set(Feature::Ask, Field::Model, "claude-opus-4-1");
        assert_eq!(s.get(Feature::Ask, Field::Model), "claude-opus-4-1", "kept as typed");
        s.cycle(Feature::Ask, Field::Model, 1);
        assert_eq!(s.get(Feature::Ask, Field::Model), MODELS[0]);
        s.reset();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn the_file_round_trips_and_tolerates_missing_or_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join(FILE);
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
        let mut s = Settings::default();
        s.set(Feature::Ask, Field::Model, "opus");
        s.set(Feature::Ask, Field::Effort, "xhigh");
        s.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"ask\": {"), "{text}");
        assert!(text.ends_with('\n'));
        assert!(!dir.path().join("deep").join("settings.json.tmp").exists());
        assert_eq!(Settings::load_from(&path).unwrap(), s);

        // A file from an older meet still carries the lookup's and the agent's pairs: they
        // are ignored.
        std::fs::write(
            &path,
            r#"{"suggest": {"model": " haiku ", "effort": ""}, "lookup": {"model": "opus", "effort": "low"}, "agent": {"model": "opus", "effort": "max"}, "future": 1}"#,
        )
        .unwrap();
        let s = Settings::load_from(&path).unwrap();
        assert_eq!(s.get(Feature::Suggest, Field::Model), "haiku");
        assert_eq!(s.get(Feature::Suggest, Field::Effort), DEFAULT);
        assert_eq!(s.ask, Settings::default().ask, "missing keys take their default");
        std::fs::write(&path, "{").unwrap();
        assert!(Settings::load_from(&path).is_err());
    }

    #[test]
    fn the_row_map_groups_by_feature_and_overrides_drop_blanks() {
        let rows = rows();
        assert_eq!(rows[0], Row::Header(Feature::Suggest));
        assert_eq!(rows[1], Row::Setting(0));
        assert_eq!(rows[2], Row::Setting(1));
        assert_eq!(rows[3], Row::Blank);
        assert_eq!(rows[4], Row::Header(Feature::Ask));
        assert_eq!(rows.len(), 4 + 2 + 1);
        assert_eq!(first_row_of(Feature::Ask), 2);
        assert_eq!(flag_name(Feature::Ask, Field::Effort), "--ask-effort");
        let mut o = Overrides::default();
        o.set(Feature::Suggest, Field::Model, Some(" haiku ".into()));
        o.set(Feature::Ask, Field::Model, Some("  ".into()));
        o.set(Feature::Ask, Field::Effort, None);
        assert_eq!(o.get(Feature::Suggest, Field::Model), Some("haiku"));
        assert_eq!(o.get(Feature::Ask, Field::Model), None);
        assert_eq!(o.get(Feature::Ask, Field::Effort), None);
    }

    #[test]
    fn a_cli_flag_wins_over_the_setting() {
        let mut s = Settings::default();
        assert_eq!(
            effective(&s, Feature::Ask, Field::Model, Some("haiku")).as_deref(),
            Some("haiku")
        );
        assert_eq!(
            effective(&s, Feature::Ask, Field::Model, Some("default")),
            None,
            "an explicit default on the command line is claude's own"
        );
        assert_eq!(
            effective(&s, Feature::Ask, Field::Model, None).as_deref(),
            Some("sonnet"),
            "no flag: the setting"
        );
        s.set(Feature::Ask, Field::Model, "default");
        assert_eq!(effective(&s, Feature::Ask, Field::Model, None), None);
        assert_eq!(effective(&s, Feature::Ask, Field::Effort, None), None);
    }
}
