use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use stack_graphs::{CancelAfterDuration, CancellationError, CancellationFlag};
use stack_graphs::arena::Handle;
use stack_graphs::graph::{File, Node, StackGraph};
use stack_graphs::partial::{PartialPath, PartialPaths};
use stack_graphs::stitching::{Database, DatabaseCandidates, ForwardPartialPathStitcher, GraphEdgeCandidates};
use tree_sitter_stack_graphs::loader::LanguageConfiguration;
use tree_sitter_stack_graphs::{CancelAfterDuration as GraphBuildDeadline, NoCancellation, Variables};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

const WALK_SKIP_DIRS: &[&str] = &["target", "node_modules", "__pycache__", "build", "dist", "vendor"];
const WALK_BUDGET: usize = 40_000;
const FILE_BYTE_LIMIT: usize = 1_000_000;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const REFRESH_DEBOUNCE: Duration = Duration::from_secs(10);
const STITCH_BUDGET_SECS_ENV: &str = "ISOHYPSE_REFS_BUDGET_SECS";
const DEFAULT_STITCH_BUDGET_SECS: u64 = 60;

type Fingerprint = (usize, u128);

fn language_for(extension: &str) -> Option<&'static str> {
    match extension {
        "py" | "pyi" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "java" => Some("java"),
        _ => None,
    }
}

fn make_configuration(language: &str) -> Option<LanguageConfiguration> {
    let configuration = match language {
        "python" => tree_sitter_stack_graphs_python::language_configuration(&NoCancellation),
        "javascript" => tree_sitter_stack_graphs_javascript::language_configuration(&NoCancellation),
        "typescript" => tree_sitter_stack_graphs_typescript::language_configuration_typescript(&NoCancellation),
        "tsx" => tree_sitter_stack_graphs_typescript::language_configuration_tsx(&NoCancellation),
        "java" => tree_sitter_stack_graphs_java::language_configuration(&NoCancellation),
        _ => return None,
    };
    Some(configuration)
}

fn locate(graph: &StackGraph, node: Handle<Node>) -> Option<(String, usize)> {
    let file = graph[node].id().file()?;
    let name = graph[file].name().to_string();
    let line = graph.source_info(node)?.span.start.line + 1;
    Some((name, line))
}

struct SourceFile {
    relative: String,
    language: &'static str,
    text: String,
}

fn walkable_directory(name: &str, path: &Path, skipped: &BTreeSet<String>) -> bool {
    !name.starts_with('.')
        && !WALK_SKIP_DIRS.contains(&name)
        && !skipped.contains(name)
        && !crate::graph::resolve::is_virtualenv(path)
}

fn collect_sources(root: &Path) -> Vec<SourceFile> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let skipped = crate::graph::resolve::load_skipped_directories(root);
    let mut budget = WALK_BUDGET;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if budget == 0 {
                return out;
            }
            budget -= 1;
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                let path = entry.path();
                if walkable_directory(name.as_ref(), &path, &skipped) {
                    stack.push(path);
                }
                continue;
            }
            let Some(extension) = name.rsplit('.').next().filter(|ext| *ext != name) else { continue };
            let Some(language) = language_for(extension) else { continue };
            let path = entry.path();
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.len() > FILE_BYTE_LIMIT {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else { continue };
            out.push(SourceFile {
                relative: relative.to_string_lossy().into_owned(),
                language,
                text,
            });
        }
    }
    out
}

fn stitch_budget() -> Duration {
    let secs = std::env::var(STITCH_BUDGET_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STITCH_BUDGET_SECS);
    Duration::from_secs(secs)
}

const STITCH_PHASE_WORK: usize = 100_000;

const FILE_BUILD_BUDGET_SECS: u64 = 10;

fn is_minimal_path(graph: &StackGraph, path: &PartialPath) -> bool {
    path.starts_at_endpoint(graph) && (path.ends_at_endpoint(graph) || path.ends_in_jump(graph))
}

fn file_paths_into_database(
    graph: &StackGraph,
    partials: &mut PartialPaths,
    database: &mut Database,
    file: Handle<File>,
    deadline: &CancelAfterDuration,
) -> Result<(), CancellationError> {
    let initial: Vec<PartialPath> = graph
        .nodes_for_file(file)
        .chain(std::iter::once(StackGraph::root_node()))
        .filter(|node| graph[*node].is_endpoint())
        .map(|node| PartialPath::from_node(graph, partials, node))
        .collect();
    let mut stitcher = ForwardPartialPathStitcher::from_partial_paths(graph, partials, initial);
    stitcher.set_check_only_join_nodes(true);
    stitcher.set_max_work_per_phase(STITCH_PHASE_WORK);
    while !stitcher.is_complete() {
        deadline.check("file partial paths")?;
        stitcher.process_next_phase(
            &mut GraphEdgeCandidates::new(graph, partials, Some(file)),
            |g, _, p| !is_minimal_path(g, p),
        );
        for path in stitcher.previous_phase_partial_paths() {
            if is_minimal_path(graph, path) {
                database.add_partial_path(graph, partials, path.clone());
            }
        }
    }
    Ok(())
}

fn build_callers(sources: &[SourceFile]) -> HashMap<(String, usize), Vec<(String, usize)>> {
    let mut configurations: HashMap<&'static str, LanguageConfiguration> = HashMap::new();
    let mut graph = StackGraph::new();
    let budget = stitch_budget();
    let graph_start = Instant::now();
    let graph_deadline = CancelAfterDuration::new(budget);
    let mut skipped = 0usize;
    for source in sources {
        if graph_deadline.check("graph build").is_err() {
            skipped += 1;
            continue;
        }
        let configuration = configurations
            .entry(source.language)
            .or_insert_with(|| make_configuration(source.language).expect("known language"));
        let file = graph.get_or_create_file(&source.relative);
        let globals = Variables::new();
        let _ = configuration.sgl.build_stack_graph_into(&mut graph, file, &source.text, &globals, &GraphBuildDeadline::new(Duration::from_secs(FILE_BUILD_BUDGET_SECS)));
    }
    let graph_elapsed = graph_start.elapsed();

    let mut partials = PartialPaths::new();
    let mut database = Database::new();
    let mut exhausted = skipped > 0;
    let paths_start = Instant::now();
    let db_deadline = CancelAfterDuration::new(budget);
    for file in graph.iter_files() {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            file_paths_into_database(&graph, &mut partials, &mut database, file, &db_deadline)
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                exhausted = true;
                break;
            }
            Err(_) => {
                eprintln!("precise refs: stitch panicked on {}, file skipped", graph[file].name());
                skipped += 1;
            }
        }
    }
    let paths_elapsed = paths_start.elapsed();

    let references: Vec<PartialPath> = graph
        .iter_nodes()
        .filter(|handle| graph[*handle].is_reference())
        .map(|node| {
            let mut path = PartialPath::from_node(&graph, &mut partials, node);
            path.eliminate_precondition_stack_variables(&mut partials);
            path
        })
        .collect();
    let mut stitcher = ForwardPartialPathStitcher::from_partial_paths(&graph, &mut partials, references);
    stitcher.set_check_only_join_nodes(true);
    stitcher.set_max_work_per_phase(STITCH_PHASE_WORK);
    let mut candidates = DatabaseCandidates::new(&graph, &mut partials, &mut database);
    let mut callers: HashMap<(String, usize), Vec<(String, usize)>> = HashMap::new();
    let stitch_start = Instant::now();
    let deadline = CancelAfterDuration::new(budget);
    while !stitcher.is_complete() {
        if deadline.check("stitching callers").is_err() {
            exhausted = true;
            break;
        }
        let phase = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stitcher.process_next_phase(&mut candidates, |_, _, _| true);
        }));
        if phase.is_err() {
            eprintln!("precise refs: stitch panicked, keeping partial index");
            exhausted = true;
            break;
        }
        for path in stitcher.previous_phase_partial_paths() {
            if !path.is_complete(&graph) {
                continue;
            }
            let (Some(reference), Some(definition)) =
                (locate(&graph, path.start_node), locate(&graph, path.end_node))
            else {
                continue;
            };
            callers.entry(definition).or_default().push(reference);
        }
    }
    eprintln!(
        "precise refs: graph {:.1}s ({} files, {} skipped), paths {:.1}s, stitch {:.1}s{}",
        graph_elapsed.as_secs_f32(),
        sources.len() - skipped,
        skipped,
        paths_elapsed.as_secs_f32(),
        stitch_start.elapsed().as_secs_f32(),
        if exhausted { ", budget exhausted, partial index" } else { "" },
    );
    for locations in callers.values_mut() {
        locations.sort();
        locations.dedup();
    }
    callers
}

fn source_fingerprint(root: &Path) -> Fingerprint {
    let mut count = 0usize;
    let mut newest = 0u128;
    let mut stack = vec![root.to_path_buf()];
    let mut budget = WALK_BUDGET;
    let skipped = crate::graph::resolve::load_skipped_directories(root);
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if budget == 0 {
                return (count, newest);
            }
            budget -= 1;
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                let path = entry.path();
                if walkable_directory(name.as_ref(), &path, &skipped) {
                    stack.push(path);
                }
                continue;
            }
            let Some(extension) = name.rsplit('.').next().filter(|ext| *ext != name) else { continue };
            if language_for(extension).is_none() {
                continue;
            }
            count += 1;
            if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                if let Ok(elapsed) = modified.duration_since(UNIX_EPOCH) {
                    newest = newest.max(elapsed.as_nanos());
                }
            }
        }
    }
    (count, newest)
}

type CallerMap = HashMap<(String, usize), Vec<(String, usize)>>;
type SharedCallers = Arc<Mutex<Option<CallerMap>>>;

const REFS_NAME_CTX: &str = "isohypse refs cache name v1";
const REFS_SEAL_CTX: &str = "isohypse refs cache seal v1";

type CachedCallers = (Fingerprint, Vec<((String, usize), Vec<(String, usize)>)>);

fn refs_cache_dir() -> PathBuf {
    crate::daemon::store_root().join("refs")
}

pub(crate) fn cache_path(root: &Path) -> Option<PathBuf> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let name = crate::objects::opaque_name(REFS_NAME_CTX, &canonical.to_string_lossy())?;
    Some(refs_cache_dir().join(format!("{name}.bin")))
}

fn load_cached_callers(root: &Path, fingerprint: Fingerprint) -> Option<CallerMap> {
    let bytes = std::fs::read(cache_path(root)?).ok()?;
    let compressed = crate::objects::open_bytes(REFS_SEAL_CTX, &bytes)?;
    let mut json = Vec::new();
    GzDecoder::new(compressed.as_slice()).read_to_end(&mut json).ok()?;
    let (cached_fingerprint, entries): CachedCallers = serde_json::from_slice(&json).ok()?;
    if cached_fingerprint != fingerprint {
        return None;
    }
    Some(entries.into_iter().collect())
}

fn store_cached_callers(root: &Path, fingerprint: Fingerprint, callers: &CallerMap) {
    use std::os::unix::fs::PermissionsExt;
    if callers.is_empty() {
        return;
    }
    let entries: Vec<(&(String, usize), &Vec<(String, usize)>)> = callers.iter().collect();
    let Ok(json) = serde_json::to_vec(&(fingerprint, entries)) else { return };
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    if encoder.write_all(&json).is_err() {
        return;
    }
    let Ok(compressed) = encoder.finish() else { return };
    let Some(sealed) = crate::objects::seal_bytes(REFS_SEAL_CTX, &compressed) else { return };
    let Some(path) = cache_path(root) else { return };
    let dir = refs_cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let _ = crate::objects::write_private(&path, &sealed);
}

fn persist_callers(root: &Path, fingerprint: Fingerprint, slot: &Mutex<Option<CallerMap>>) {
    let Ok(guard) = slot.lock() else { return };
    let Some(map) = guard.as_ref() else { return };
    store_cached_callers(root, fingerprint, map);
}

fn rebuild(root: &Path, slot: &Mutex<Option<CallerMap>>) {
    let sources = collect_sources(root);
    let built = if sources.is_empty() {
        HashMap::new()
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_callers(&sources))).unwrap_or_else(|_| {
            eprintln!("precise refs: overlay build panicked, serving without precise refs");
            HashMap::new()
        })
    };
    if !sources.is_empty() {
        eprintln!("precise refs: {} definitions resolved across {} files", built.len(), sources.len());
    }
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(built);
    }
}

fn refresh_loop(root: PathBuf, slot: SharedCallers) {
    let mut indexed = source_fingerprint(&root);
    match load_cached_callers(&root, indexed) {
        Some(cached) => {
            eprintln!("precise refs: {} definitions loaded from cache", cached.len());
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(cached);
            }
        }
        None => {
            rebuild(&root, &slot);
            indexed = source_fingerprint(&root);
            persist_callers(&root, indexed, &slot);
        }
    }
    let mut pending: Option<(Fingerprint, Instant)> = None;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let current = source_fingerprint(&root);
        if current == indexed {
            pending = None;
            continue;
        }
        match pending {
            Some((mark, since)) if mark == current => {
                if since.elapsed() >= REFRESH_DEBOUNCE {
                    rebuild(&root, &slot);
                    indexed = current;
                    pending = None;
                    persist_callers(&root, indexed, &slot);
                }
            }
            _ => pending = Some((current, Instant::now())),
        }
    }
}

pub struct PreciseIndex {
    callers: SharedCallers,
}

impl PreciseIndex {
    pub fn spawn(root: PathBuf, sessions: Arc<crate::session::SessionRegistry>, workspace: String) -> PreciseIndex {
        let callers = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&callers);
        let notify_slot = Arc::clone(&callers);
        std::thread::spawn(move || refresh_loop(root, slot));
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if notify_slot.lock().map(|guard| guard.is_some()).unwrap_or(false) {
                sessions.publish("precise-ready", Some(&workspace), &serde_json::json!({ "workspace": workspace }));
                break;
            }
        });
        PreciseIndex { callers }
    }

    pub fn ready(&self) -> bool {
        self.callers.lock().map(|guard| guard.is_some()).unwrap_or(false)
    }

    pub fn references(&self, file: &str, line: usize, _column: usize) -> Option<Vec<(String, usize)>> {
        let guard = self.callers.lock().ok()?;
        let callers = guard.as_ref()?;
        callers.get(&(file.to_string(), line)).cloned()
    }
}
