use self::resources::ResourcesState;
use crate::{
    intern::{self, InternedStr},
    view,
    warnings::Linter,
};
use console_api as proto;
use std::{
    cell::RefCell,
    collections::HashMap,
    convert::TryInto,
    fmt,
    io::Cursor,
    rc::Rc,
    time::{Duration, SystemTime},
};
use tasks::{Details, Task, TasksState};
use tui::{
    style::{Color, Modifier},
    text::Span,
};

pub mod resources;
pub mod tasks;

pub(crate) type DetailsRef = Rc<RefCell<Option<Details>>>;

#[derive(Default, Debug)]
pub(crate) struct State {
    metas: HashMap<u64, Metadata>,
    last_updated_at: Option<SystemTime>,
    temporality: Temporality,
    tasks_state: TasksState,
    resources_state: ResourcesState,
    current_task_details: DetailsRef,
    retain_for: Option<Duration>,
    strings: intern::Strings,
}
pub(crate) enum Visibility {
    Show,
    Hide,
}

#[derive(Debug)]
pub(crate) struct Metadata {
    field_names: Vec<InternedStr>,
    target: InternedStr,
    id: u64,
    //TODO: add more metadata as needed
}

#[derive(Debug)]
pub(crate) struct Field {
    pub(crate) name: InternedStr,
    pub(crate) value: FieldValue,
}

#[derive(Debug)]
pub(crate) enum FieldValue {
    Bool(bool),
    Str(String),
    U64(u64),
    I64(i64),
    Debug(String),
}

#[derive(Debug)]
enum Temporality {
    Live,
    Paused,
}

impl State {
    pub(crate) fn with_retain_for(mut self, retain_for: Option<Duration>) -> Self {
        self.retain_for = retain_for;
        self
    }

    pub(crate) fn with_task_linters(
        mut self,
        linters: impl IntoIterator<Item = Linter<Task>>,
    ) -> Self {
        self.tasks_state.linters.extend(linters.into_iter());
        self
    }

    pub(crate) fn last_updated_at(&self) -> Option<SystemTime> {
        self.last_updated_at
    }

    pub(crate) fn update(
        &mut self,
        styles: &view::Styles,
        current_view: &view::ViewState,
        update: proto::instrument::Update,
    ) {
        if let Some(now) = update.now.map(|v| v.try_into().unwrap()) {
            self.last_updated_at = Some(now);
        }

        let strings = &mut self.strings;
        if let Some(new_metadata) = update.new_metadata {
            let metas = new_metadata.metadata.into_iter().filter_map(|meta| {
                let id = meta.id?.id;
                let metadata = meta.metadata?;
                Some((id, Metadata::from_proto(metadata, id, strings)))
            });
            self.metas.extend(metas);
        }

        if let Some(tasks_update) = update.task_update {
            let visibility = if matches!(current_view, view::ViewState::TasksList) {
                Visibility::Show
            } else {
                Visibility::Hide
            };
            self.tasks_state.update_tasks(
                styles,
                &mut self.strings,
                &self.metas,
                tasks_update,
                visibility,
            )
        }

        if let Some(resources_update) = update.resource_update {
            let visibility = if matches!(current_view, view::ViewState::ResourcesList) {
                Visibility::Show
            } else {
                Visibility::Hide
            };
            self.resources_state.update_resources(
                styles,
                &mut self.strings,
                &self.metas,
                resources_update,
                visibility,
            )
        }
    }

    pub(crate) fn retain_active(&mut self) {
        if self.is_paused() {
            return;
        }

        if let (Some(now), Some(retain_for)) = (self.last_updated_at(), self.retain_for) {
            self.tasks_state.retain_active(now, retain_for);
            self.resources_state.retain_active(now, retain_for);
        }

        // After dropping idle tasks & resources, prune any interned strings
        // that are no longer referenced.
        self.strings.retain_referenced();
    }

    pub(crate) fn task_details_ref(&self) -> DetailsRef {
        self.current_task_details.clone()
    }

    pub(crate) fn tasks_state(&mut self) -> &TasksState {
        &self.tasks_state
    }

    pub(crate) fn tasks_state_mut(&mut self) -> &mut TasksState {
        &mut self.tasks_state
    }

    pub(crate) fn resources_state_mut(&mut self) -> &mut ResourcesState {
        &mut self.resources_state
    }

    pub(crate) fn update_task_details(&mut self, update: proto::tasks::TaskDetails) {
        if let Some(id) = update.task_id {
            let details = Details {
                task_id: id.id,
                poll_times_histogram: update.poll_times_histogram.and_then(|data| {
                    hdrhistogram::serialization::Deserializer::new()
                        .deserialize(&mut Cursor::new(&data))
                        .ok()
                }),
                // last_updated_at: update.now.map(|now| now.try_into().unwrap()),
            };

            *self.current_task_details.borrow_mut() = Some(details);
        }
    }

    pub(crate) fn unset_task_details(&mut self) {
        *self.current_task_details.borrow_mut() = None;
    }

    // temporality methods

    pub(crate) fn pause(&mut self) {
        self.temporality = Temporality::Paused;
    }

    pub(crate) fn resume(&mut self) {
        self.temporality = Temporality::Live;
    }

    pub(crate) fn is_paused(&self) -> bool {
        matches!(self.temporality, Temporality::Paused)
    }
}

impl Default for Temporality {
    fn default() -> Self {
        Self::Live
    }
}

impl Metadata {
    fn from_proto(pb: proto::Metadata, id: u64, strings: &mut intern::Strings) -> Self {
        Self {
            field_names: pb
                .field_names
                .into_iter()
                .map(|n| strings.string(n))
                .collect(),
            target: strings.string(pb.target),
            id,
        }
    }
}

// === impl Field ===

impl Field {
    const SPAWN_LOCATION: &'static str = "spawn.location";
    const NAME: &'static str = "task.name";

    /// Converts a wire-format `Field` into an internal `Field` representation,
    /// using the provided `Metadata` for the task span that the field came
    /// from.
    ///
    /// If the field is invalid or it has a string value which is empty, this
    /// returns `None`.
    fn from_proto(
        proto::Field {
            name,
            metadata_id,
            value,
        }: proto::Field,
        meta: &Metadata,
        strings: &mut intern::Strings,
    ) -> Option<Self> {
        use proto::field::Name;
        let name = match name? {
            Name::StrName(n) => strings.string(n),
            Name::NameIdx(idx) => {
                let meta_id = metadata_id.map(|m| m.id);
                if meta_id != Some(meta.id) {
                    tracing::warn!(
                        task.meta_id = meta.id,
                        field.meta.id = ?meta_id,
                        field.name_index = idx,
                        ?meta,
                        "skipping malformed field name (metadata id mismatch)"
                    );
                    debug_assert_eq!(
                        meta_id,
                        Some(meta.id),
                        "malformed field name: metadata ID mismatch! (name idx={}; metadata={:#?})",
                        idx,
                        meta,
                    );
                    return None;
                }
                match meta.field_names.get(idx as usize).cloned() {
                    Some(name) => name,
                    None => {
                        tracing::warn!(
                            task.meta_id = meta.id,
                            field.meta.id = ?meta_id,
                            field.name_index = idx,
                            ?meta,
                            "missing field name for index"
                        );
                        return None;
                    }
                }
            }
        };

        debug_assert!(
            value.is_some(),
            "missing field value for field `{:?}` (metadata={:#?})",
            name,
            meta,
        );
        let mut value = FieldValue::from(value?)
            // if the value is an empty string, just skip it.
            .ensure_nonempty()?;

        if &*name == Field::SPAWN_LOCATION {
            value = value.truncate_registry_path();
        }

        Some(Self { name, value })
    }

    fn make_formatted(styles: &view::Styles, fields: &mut Vec<Field>) -> Vec<Vec<Span<'static>>> {
        use std::cmp::Ordering;

        let key_style = styles.fg(Color::LightBlue).add_modifier(Modifier::BOLD);
        let delim_style = styles.fg(Color::LightBlue).add_modifier(Modifier::DIM);
        let val_style = styles.fg(Color::Yellow);

        fields.sort_unstable_by(|left, right| {
            if &*left.name == Field::NAME {
                return Ordering::Less;
            }

            if &*right.name == Field::NAME {
                return Ordering::Greater;
            }

            if &*left.name == Field::SPAWN_LOCATION {
                return Ordering::Greater;
            }

            if &*right.name == Field::SPAWN_LOCATION {
                return Ordering::Less;
            }

            left.name.cmp(&right.name)
        });

        let mut formatted = Vec::with_capacity(fields.len());
        let mut fields = fields.iter();
        if let Some(field) = fields.next() {
            formatted.push(vec![
                Span::styled(field.name.to_string(), key_style),
                Span::styled("=", delim_style),
                Span::styled(format!("{} ", field.value), val_style),
            ]);
            for field in fields {
                formatted.push(vec![
                    // Span::styled(", ", delim_style),
                    Span::styled(field.name.to_string(), key_style),
                    Span::styled("=", delim_style),
                    Span::styled(format!("{} ", field.value), val_style),
                ])
            }
        }
        formatted
    }
}

// === impl FieldValue ===

impl fmt::Display for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldValue::Bool(v) => fmt::Display::fmt(v, f)?,
            FieldValue::Str(v) => fmt::Display::fmt(v, f)?,
            FieldValue::U64(v) => fmt::Display::fmt(v, f)?,
            FieldValue::Debug(v) => fmt::Display::fmt(v, f)?,
            FieldValue::I64(v) => fmt::Display::fmt(v, f)?,
        }

        Ok(())
    }
}

impl FieldValue {
    /// Truncates paths including `.cargo/registry`.
    fn truncate_registry_path(self) -> Self {
        match self {
            FieldValue::Str(s) | FieldValue::Debug(s) => {
                FieldValue::Debug(truncate_registry_path(s))
            }

            f => f,
        }
    }

    /// If `self` is an empty string, returns `None`. Otherwise, returns `Some(self)`.
    fn ensure_nonempty(self) -> Option<Self> {
        match self {
            FieldValue::Debug(s) | FieldValue::Str(s) if s.is_empty() => None,
            val => Some(val),
        }
    }
}

impl From<proto::field::Value> for FieldValue {
    fn from(pb: proto::field::Value) -> Self {
        match pb {
            proto::field::Value::BoolVal(v) => Self::Bool(v),
            proto::field::Value::StrVal(v) => Self::Str(v),
            proto::field::Value::I64Val(v) => Self::I64(v),
            proto::field::Value::U64Val(v) => Self::U64(v),
            proto::field::Value::DebugVal(v) => Self::Debug(v),
        }
    }
}

fn truncate_registry_path(s: String) -> String {
    use once_cell::sync::OnceCell;
    use regex::Regex;
    use std::borrow::Cow;

    static REGEX: OnceCell<Regex> = OnceCell::new();
    let regex = REGEX.get_or_init(|| {
        Regex::new(r#".*/\.cargo(/registry/src/[^/]*/|/git/checkouts/)"#)
            .expect("failed to compile regex")
    });

    return match regex.replace(&s, "<cargo>/") {
        Cow::Owned(s) => s,
        // String was not modified, return the original.
        Cow::Borrowed(_) => s.to_string(),
    };
}

fn format_location(loc: Option<proto::Location>) -> String {
    loc.map(|mut l| {
        if let Some(file) = l.file.take() {
            let truncated = truncate_registry_path(file);
            l.file = Some(truncated);
        }
        format!("{} ", l)
    })
    .unwrap_or_else(|| "<unknown location>".to_string())
}
