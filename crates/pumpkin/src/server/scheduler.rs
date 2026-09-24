use crate::plugin::loader::wasm::wasm_host::WasmPlugin;
use crate::server::Server;
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, Weak};

pub type TaskId = u32;

pub struct ScheduledTask {
    pub id: TaskId,
    pub plugin: Arc<WasmPlugin>,
    pub handler_id: u32,
    pub next_tick: u64,
    pub period: Option<u64>,
}

impl PartialEq for ScheduledTask {
    fn eq(&self, other: &Self) -> bool {
        self.next_tick == other.next_tick
    }
}

impl Eq for ScheduledTask {}

impl PartialOrd for ScheduledTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order so BinaryHeap is a min-heap
        other.next_tick.cmp(&self.next_tick)
    }
}

pub struct TaskScheduler {
    tasks: Mutex<BinaryHeap<ScheduledTask>>,
    cancelled_tasks: Mutex<HashSet<TaskId>>,
    disabled_plugins: Mutex<Vec<Weak<WasmPlugin>>>,
    next_task_id: std::sync::atomic::AtomicU32,
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(BinaryHeap::new()),
            cancelled_tasks: Mutex::new(HashSet::new()),
            disabled_plugins: Mutex::new(Vec::new()),
            next_task_id: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn schedule_delayed_task(
        &self,
        plugin: Arc<WasmPlugin>,
        handler_id: u32,
        delay: u64,
        current_tick: u64,
    ) -> TaskId {
        let id = self.next_task_id.fetch_add(1, AtomicOrdering::SeqCst);
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Self::is_plugin_disabled(&mut disabled_plugins, &plugin) {
            return id;
        }
        let task = ScheduledTask {
            id,
            plugin,
            handler_id,
            next_tick: current_tick + delay,
            period: None,
        };
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        id
    }

    pub fn schedule_repeating_task(
        &self,
        plugin: Arc<WasmPlugin>,
        handler_id: u32,
        delay: u64,
        period: u64,
        current_tick: u64,
    ) -> TaskId {
        let id = self.next_task_id.fetch_add(1, AtomicOrdering::SeqCst);
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Self::is_plugin_disabled(&mut disabled_plugins, &plugin) {
            return id;
        }
        let task = ScheduledTask {
            id,
            plugin,
            handler_id,
            next_tick: current_tick + delay,
            period: Some(period),
        };
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        id
    }

    pub fn cancel_task(&self, id: TaskId) {
        self.cancelled_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub fn disable_plugin(&self, plugin: &Arc<WasmPlugin>) {
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Self::is_plugin_disabled(&mut disabled_plugins, plugin) {
            disabled_plugins.push(Arc::downgrade(plugin));
        }

        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|task| !Arc::ptr_eq(&task.plugin, plugin));
    }

    fn is_plugin_disabled(
        disabled_plugins: &mut Vec<Weak<WasmPlugin>>,
        plugin: &Arc<WasmPlugin>,
    ) -> bool {
        disabled_plugins.retain(|entry| entry.strong_count() > 0);
        let plugin = Arc::downgrade(plugin);
        disabled_plugins
            .iter()
            .any(|entry| Weak::ptr_eq(entry, &plugin))
    }

    pub fn tick(&self, server: &Arc<Server>) {
        let current_tick = server.tick_count.load(AtomicOrdering::Relaxed) as u64;
        let mut tasks_to_run = Vec::new();

        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut cancelled = self
                .cancelled_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            while let Some(task) = tasks.peek() {
                if task.next_tick > current_tick {
                    break;
                }

                let Some(task) = tasks.pop() else {
                    break;
                };
                if cancelled.remove(&task.id) {
                    continue;
                }

                tasks_to_run.push(task);
            }
        }

        for mut task in tasks_to_run {
            let mut disabled_plugins = self
                .disabled_plugins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if Self::is_plugin_disabled(&mut disabled_plugins, &task.plugin) {
                continue;
            }
            drop(disabled_plugins);

            // Run the task
            let plugin = task.plugin.clone();
            let handler_id = task.handler_id;
            let server_clone = server.clone();

            server.spawn_task(async move {
                let function = match plugin.plugin_instance.as_ref() {
                    crate::plugin::loader::wasm::wasm_host::PluginInstance::V0_1(instance) => {
                        instance.func_handle_task()
                    }
                };
                if let Err(error) = plugin
                    .store
                    .call_guest(move |mut guest| {
                        Box::pin(async move {
                            let (server_resource, server_rep) = guest.with(|mut store| {
                                let resource = store.data_mut().add_server(server_clone)?;
                                let rep = resource.rep();
                                Ok::<_, wasmtime::Error>((resource, rep))
                            })?;
                            let result = guest.call(function, (handler_id, server_resource)).await;
                            guest.with(|mut store| {
                                let _ = store.data_mut().resource_table.delete::<
                                    crate::plugin::loader::wasm::wasm_host::state::ServerResource,
                                >(wasmtime::component::Resource::new_own(server_rep));
                            });
                            result
                        })
                    })
                    .await
                {
                    tracing::error!(handler_id, %error, "Wasm scheduled task failed");
                }
            });

            // If repeating, schedule next run
            if let Some(period) = task.period {
                task.next_tick = current_tick + period;
                let mut disabled_plugins = self
                    .disabled_plugins
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !Self::is_plugin_disabled(&mut disabled_plugins, &task.plugin) {
                    self.tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(task);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledFunctionEvent {
    pub id: String,
    pub trigger_tick: u64,
    pub function_name: String,
    pub is_tag: bool,
}

impl PartialOrd for ScheduledFunctionEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledFunctionEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other.trigger_tick.cmp(&self.trigger_tick)
    }
}

pub struct ScheduledFunctionQueue {
    queue: Mutex<BinaryHeap<ScheduledFunctionEvent>>,
    persistence_enabled: AtomicBool,
}

impl Default for ScheduledFunctionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduledFunctionQueue {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: Mutex::new(BinaryHeap::new()),
            persistence_enabled: AtomicBool::new(true),
        }
    }

    pub fn disable_persistence(&self) {
        self.persistence_enabled
            .store(false, AtomicOrdering::Relaxed);
    }

    /// Returns only currently pending events, excluding removed and executed events.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ScheduledFunctionEvent> {
        let queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut events: Vec<_> = queue.iter().cloned().collect();
        events.sort_by(|a, b| {
            a.trigger_tick
                .cmp(&b.trigger_tick)
                .then_with(|| a.id.cmp(&b.id))
        });
        events
    }

    /// Replaces the pending queue with a previously captured snapshot.
    pub fn restore(&self, events: impl IntoIterator<Item = ScheduledFunctionEvent>) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *queue = events.into_iter().collect();
    }

    /// Loads `data/scheduled_events.dat`, returning an empty queue when it does not exist.
    pub fn load_from_world_dir(world_dir: &Path) -> Result<Self, String> {
        let path = world_dir.join("data").join("scheduled_events.dat");
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => return Err(error.to_string()),
        }
        let file = File::open(&path).map_err(|error| error.to_string())?;
        let root = pumpkin_nbt::nbt_compress::read_gzip_compound_tag(file)
            .map_err(|error| error.to_string())?;
        Self::from_nbt(&root)
    }

    /// Saves pending events to the vanilla `data/scheduled_events.dat` location.
    pub fn save_to_world_dir(&self, world_dir: &Path) -> Result<(), String> {
        if !self.persistence_enabled.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }
        let nbt = self.to_nbt()?;
        let data_dir = world_dir.join("data");
        fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        let file = File::create(data_dir.join("scheduled_events.dat"))
            .map_err(|error| error.to_string())?;
        pumpkin_nbt::nbt_compress::write_gzip_compound_tag(nbt, file)
            .map_err(|error| error.to_string())
    }

    /// Encodes pending events using the vanilla scheduled_events.dat NBT shape.
    pub fn to_nbt(&self) -> Result<NbtCompound, String> {
        let events = self
            .snapshot()
            .into_iter()
            .map(|event| {
                let trigger_time = i64::try_from(event.trigger_tick)
                    .map_err(|_| "scheduled event trigger time exceeds NBT long".to_owned())?;
                let mut callback = NbtCompound::new();
                callback.put_string(
                    "type",
                    if event.is_tag {
                        "minecraft:function_tag"
                    } else {
                        "minecraft:function"
                    }
                    .to_owned(),
                );
                callback.put_string(
                    "id",
                    if event.is_tag {
                        event
                            .function_name
                            .strip_prefix('#')
                            .unwrap_or(&event.function_name)
                    } else {
                        &event.function_name
                    }
                    .to_owned(),
                );
                let mut entry = NbtCompound::new();
                entry.put_long("trigger_time", trigger_time);
                entry.put_string("id", event.id);
                entry.put_compound("callback", callback);
                Ok(NbtTag::Compound(entry))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut data = NbtCompound::new();
        data.put_list("events", events);
        let mut root = NbtCompound::new();
        root.put_int(
            "DataVersion",
            pumpkin_world::world_info::MAXIMUM_SUPPORTED_WORLD_DATA_VERSION,
        );
        root.put_compound("data", data);
        Ok(root)
    }

    /// Decodes vanilla-shaped event data, rejecting malformed entries and callback types.
    pub fn from_nbt(root: &NbtCompound) -> Result<Self, String> {
        root.get_int("DataVersion")
            .ok_or_else(|| "missing or invalid scheduled events DataVersion".to_owned())?;
        let data = root
            .get_compound("data")
            .ok_or_else(|| "missing or invalid scheduled events data compound".to_owned())?;
        let list = data
            .get_list("events")
            .ok_or_else(|| "missing or invalid scheduled events list".to_owned())?;
        let mut events = Vec::with_capacity(list.len());
        for tag in list {
            let NbtTag::Compound(entry) = tag else {
                return Err("scheduled event must be a compound".to_owned());
            };
            let trigger_tick = entry
                .get_long("trigger_time")
                .and_then(|time| u64::try_from(time).ok())
                .ok_or_else(|| "missing or invalid scheduled event trigger_time".to_owned())?;
            let id = entry
                .get_string("id")
                .ok_or_else(|| "missing or invalid scheduled event id".to_owned())?
                .to_owned();
            let callback = entry
                .get_compound("callback")
                .ok_or_else(|| "missing or invalid scheduled event callback".to_owned())?;
            let callback_id = callback
                .get_string("id")
                .ok_or_else(|| "missing or invalid scheduled event callback id".to_owned())?;
            let (function_name, is_tag) = match callback.get_string("type") {
                Some("minecraft:function") => (callback_id.to_owned(), false),
                Some("minecraft:function_tag") => (format!("#{callback_id}"), true),
                _ => return Err("unknown or invalid scheduled event callback type".to_owned()),
            };
            events.push(ScheduledFunctionEvent {
                id,
                trigger_tick,
                function_name,
                is_tag,
            });
        }
        let queue = Self::new();
        queue.restore(events);
        Ok(queue)
    }

    pub fn schedule(
        &self,
        id: String,
        trigger_tick: u64,
        function_name: String,
        is_tag: bool,
        replace: bool,
    ) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if replace {
            let mut retained = Vec::new();
            while let Some(event) = queue.pop() {
                if event.id != id {
                    retained.push(event);
                }
            }
            for event in retained {
                queue.push(event);
            }
        }
        queue.push(ScheduledFunctionEvent {
            id,
            trigger_tick,
            function_name,
            is_tag,
        });
    }

    pub fn remove(&self, id: &str) -> usize {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut count = 0;
        let mut retained = Vec::new();
        while let Some(event) = queue.pop() {
            if event.id == id {
                count += 1;
            } else {
                retained.push(event);
            }
        }
        for event in retained {
            queue.push(event);
        }
        count
    }

    #[must_use]
    pub fn get_event_ids(&self) -> Vec<String> {
        let queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = queue.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    fn take_due_events(&self, current_tick: u64) -> Vec<ScheduledFunctionEvent> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut to_run = Vec::new();
        while let Some(event) = queue.peek() {
            if event.trigger_tick > current_tick {
                break;
            }
            if let Some(event) = queue.pop() {
                to_run.push(event);
            }
        }
        to_run
    }

    pub fn tick(&self, server: &Arc<Server>, current_tick: u64) {
        for event in self.take_due_events(current_tick) {
            let _ = crate::data::datapack::DatapackManager::execute_function_from_console(
                server,
                &event.function_name,
            );
        }
    }
}

#[cfg(test)]
mod scheduled_function_queue_tests {
    use super::*;

    fn event(id: &str, time: u64, name: &str, is_tag: bool) -> ScheduledFunctionEvent {
        ScheduledFunctionEvent {
            id: id.into(),
            trigger_tick: time,
            function_name: name.into(),
            is_tag,
        }
    }

    #[test]
    fn queue_snapshot_restore_preserves_pending_event_identity() {
        let queue = ScheduledFunctionQueue::new();
        queue.schedule("f".into(), 123, "minecraft:foo".into(), false, false);
        queue.schedule("t".into(), 456, "#minecraft:bar".into(), true, false);
        let snapshot = queue.snapshot();
        let restored = ScheduledFunctionQueue::new();
        restored.restore(snapshot.clone());
        assert_eq!(restored.snapshot(), snapshot);
    }

    #[test]
    fn vanilla_nbt_round_trip_and_schema() {
        let queue = ScheduledFunctionQueue::new();
        queue.schedule(
            "function-id".into(),
            123,
            "minecraft:foo".into(),
            false,
            false,
        );
        queue.schedule("tag-id".into(), 456, "#minecraft:bar".into(), true, false);
        let nbt = queue.to_nbt().unwrap();
        assert_eq!(
            nbt.get_int("DataVersion"),
            Some(pumpkin_world::world_info::MAXIMUM_SUPPORTED_WORLD_DATA_VERSION)
        );
        let entries = nbt
            .get_compound("data")
            .unwrap()
            .get_list("events")
            .unwrap();
        assert_eq!(entries.len(), 2);
        let NbtTag::Compound(function) = &entries[0] else {
            panic!("expected compound");
        };
        assert_eq!(function.get_long("trigger_time"), Some(123));
        assert_eq!(function.get_string("id"), Some("function-id"));
        let callback = function.get_compound("callback").unwrap();
        assert_eq!(callback.get_string("type"), Some("minecraft:function"));
        assert_eq!(callback.get_string("id"), Some("minecraft:foo"));
        let NbtTag::Compound(tag) = &entries[1] else {
            panic!("expected compound");
        };
        assert_eq!(tag.get_long("trigger_time"), Some(456));
        let callback = tag.get_compound("callback").unwrap();
        assert_eq!(callback.get_string("type"), Some("minecraft:function_tag"));
        assert_eq!(callback.get_string("id"), Some("minecraft:bar"));
        assert_eq!(
            ScheduledFunctionQueue::from_nbt(&nbt).unwrap().snapshot(),
            queue.snapshot()
        );
    }

    #[test]
    fn empty_removed_replaced_and_executed_events_are_not_snapshotted() {
        let queue = ScheduledFunctionQueue::new();
        assert!(
            queue
                .to_nbt()
                .unwrap()
                .get_compound("data")
                .unwrap()
                .get_list("events")
                .unwrap()
                .is_empty()
        );
        queue.schedule("replace".into(), 1, "minecraft:old".into(), false, false);
        queue.schedule("replace".into(), 5, "minecraft:new".into(), false, true);
        queue.schedule("remove".into(), 3, "minecraft:removed".into(), false, false);
        queue.schedule(
            "execute".into(),
            4,
            "minecraft:executed".into(),
            false,
            false,
        );
        assert_eq!(queue.remove("remove"), 1);
        assert_eq!(queue.take_due_events(4).len(), 1);
        assert_eq!(
            queue.snapshot(),
            vec![event("replace", 5, "minecraft:new", false)]
        );
    }

    #[test]
    fn world_file_save_reconstruct_load_and_tick_preserves_absolute_function_and_tag_events() {
        let world_dir = std::env::temp_dir().join(format!(
            "pumpkin-scheduled-events-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let original = ScheduledFunctionQueue::new();
        original.schedule("function".into(), 120, "minecraft:one".into(), false, false);
        original.schedule("tag".into(), 180, "#minecraft:two".into(), true, false);
        original.save_to_world_dir(&world_dir).unwrap();

        let path = world_dir.join("data").join("scheduled_events.dat");
        assert!(path.exists());
        assert!(
            !world_dir
                .join("data")
                .join("minecraft")
                .join("scheduled_events.dat")
                .exists()
        );
        let restored = ScheduledFunctionQueue::load_from_world_dir(&world_dir).unwrap();
        assert_eq!(restored.snapshot(), original.snapshot());
        assert!(restored.take_due_events(119).is_empty());
        assert_eq!(
            restored.take_due_events(120),
            vec![event("function", 120, "minecraft:one", false)]
        );
        assert_eq!(
            restored.snapshot(),
            vec![event("tag", 180, "#minecraft:two", true)]
        );
        std::fs::remove_dir_all(world_dir).unwrap();
    }

    #[test]
    fn malformed_world_file_is_preserved_when_persistence_is_disabled() {
        let world_dir = std::env::temp_dir().join(format!(
            "pumpkin-scheduled-events-malformed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = world_dir.join("data").join("scheduled_events.dat");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b"recoverable malformed scheduled events";
        std::fs::write(&path, original).unwrap();

        assert!(ScheduledFunctionQueue::load_from_world_dir(&world_dir).is_err());
        let queue = ScheduledFunctionQueue::new();
        queue.disable_persistence();
        queue.schedule("new".into(), 10, "minecraft:new".into(), false, false);
        queue.save_to_world_dir(&world_dir).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(world_dir).unwrap();
    }

    #[test]
    fn missing_world_file_allows_initial_empty_save() {
        let world_dir = std::env::temp_dir().join(format!(
            "pumpkin-scheduled-events-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = world_dir.join("data").join("scheduled_events.dat");
        assert!(ScheduledFunctionQueue::load_from_world_dir(&world_dir).is_ok());
        ScheduledFunctionQueue::new()
            .save_to_world_dir(&world_dir)
            .unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(world_dir).unwrap();
    }

    #[test]
    fn unknown_callback_type_is_rejected() {
        let mut callback = NbtCompound::new();
        callback.put_string("type", "minecraft:unknown".into());
        callback.put_string("id", "minecraft:x".into());
        let mut entry = NbtCompound::new();
        entry.put_long("trigger_time", 0);
        entry.put_string("id", "id".into());
        entry.put_compound("callback", callback);
        let mut data = NbtCompound::new();
        data.put_list("events", vec![NbtTag::Compound(entry)]);
        let mut root = NbtCompound::new();
        root.put_int(
            "DataVersion",
            pumpkin_world::world_info::MAXIMUM_SUPPORTED_WORLD_DATA_VERSION,
        );
        root.put_compound("data", data);
        assert!(ScheduledFunctionQueue::from_nbt(&root).is_err());
    }
}
