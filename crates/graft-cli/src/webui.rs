//! Loopback-only host for the embedded React worktree viewer.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    fs,
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Args;
use graft::repo::{RepoPathContentState, Repository};
use graft_sdk::{
    CommitChangedPathsOptions, InventoryKind, InventoryOptions, ReadPathContentOptions,
    RepositorySession, SqliteDiffPathsOptions, SqliteDiffResponse,
};
use graft_sdk::{
    ContinueMergeOptions, ListMergeConflictsOptions, ListMergePathsOptions, MergePathFilter,
    MergePathResult, MergeStatus, MergeVersion, ReadMergeVersionOptions, ResolveMergeCellOptions,
    ResolveMergeRowOptions, SetMergePathResultOptions, WriteAndStageTextResultOptions,
};
use graft_sqlite::repo_service::{RepositoryCommand, RepositoryCommandService};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_TEXT: u64 = 2 * 1024 * 1024;
const MAX_REQUEST: u64 = MAX_TEXT * 6 + 4096;
const PAGE_SIZE: usize = 100;
const MAX_TREE_PATHS: usize = 20_000;

mod loading;
mod preview;

struct EmbeddedAsset {
    path: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

include!("../../../web-ui/generated/assets.rs");

fn embedded_asset(path: &str) -> Option<&'static EmbeddedAsset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

fn asset_cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    }
}

#[derive(Args)]
pub struct UiArgs {
    /// Folder inside an existing Graft repository.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Loopback port; by default the OS chooses an available port.
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Print the URL without launching the system browser.
    #[arg(long)]
    no_open: bool,
}

pub fn arguments() -> Vec<OsString> {
    directory_arguments(std::env::args_os().collect())
}

fn directory_arguments(mut args: Vec<OsString>) -> Vec<OsString> {
    // Only an existing folder is a shorthand. Normal subcommands and flags
    // retain clap's existing behavior, including errors for unknown commands.
    if args.get(1).is_some_and(|arg| Path::new(arg).is_dir()) {
        let commands =
            <crate::Command as clap::Subcommand>::augment_subcommands(clap::Command::new("graft"));
        if args[1]
            .to_str()
            .is_none_or(|name| commands.find_subcommand(name).is_none())
        {
            args.insert(1, OsString::from("ui"));
        }
    }
    args
}

pub fn run(args: UiArgs) -> Result<()> {
    let loading = loading::StartupLoading::start();
    let viewer = Viewer::open(&args.path)?;
    let server = Server::http(("127.0.0.1", args.port))
        .map_err(|error| anyhow::anyhow!("cannot start local viewer: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("missing listener address")?
        .port();
    let origin = format!("http://127.0.0.1:{port}");
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret)
        .map_err(|error| anyhow::anyhow!("cannot create session: {error}"))?;
    let token = secret
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let url = format!("{origin}/#token={token}");
    drop(loading);
    println!(
        "Graft · {}\n{url}\nPress Ctrl+C to stop.",
        viewer.root.display()
    );
    if !args.no_open
        && let Err(error) = open_browser(&url)
    {
        eprintln!("Could not open browser: {error}. Open the URL above.");
    }
    for request in server.incoming_requests() {
        handle_request(request, &viewer, &origin, &token);
    }
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status()?;
    #[cfg(target_os = "windows")]
    let status = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .status()?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let status = Command::new("xdg-open").arg(url).status()?;
    if !status.success() {
        bail!("browser launcher exited with {status}");
    }
    Ok(())
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn authorized(
    host: Option<&str>,
    origin: Option<&str>,
    auth: Option<&str>,
    expected: &str,
    token: &str,
) -> bool {
    host == expected.strip_prefix("http://")
        && origin.is_none_or(|value| value == expected)
        && auth.is_some_and(|value| value.strip_prefix("Bearer ") == Some(token))
}

fn handle_request(mut request: Request, viewer: &Viewer, origin: &str, token: &str) {
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if header(&request, "Host") != origin.strip_prefix("http://") {
        respond(
            request,
            403,
            "application/json",
            br#"{"error":"Invalid host"}"#.to_vec(),
        );
        return;
    }
    if !path.starts_with("/api/") {
        serve_asset(request, &path);
        return;
    }
    if let Some(encoded) = path.strip_prefix("/api/preview/") {
        let query_token = request.url().split_once("?token=").map(|(_, value)| value);
        let auth = query_token.map(|value| format!("Bearer {value}"));
        if request.method() != &Method::Get
            || !authorized(
                header(&request, "Host"),
                header(&request, "Origin"),
                auth.as_deref(),
                origin,
                token,
            )
        {
            respond(
                request,
                403,
                "text/plain",
                b"Preview not authorized".to_vec(),
            );
        } else {
            preview::serve(request, viewer, encoded);
        }
        return;
    }
    if !authorized(
        header(&request, "Host"),
        header(&request, "Origin"),
        header(&request, "Authorization"),
        origin,
        token,
    ) {
        respond(
            request,
            403,
            "application/json",
            br#"{"error":"This local session is not authorized. Open the URL printed by graft."}"#
                .to_vec(),
        );
        return;
    }
    if request.method() != &Method::Post {
        respond(
            request,
            405,
            "application/json",
            br#"{"error":"POST required"}"#.to_vec(),
        );
        return;
    }
    let result = (|| -> Result<Value> {
        let input = read_input(&mut request)?;
        viewer.dispatch(&path, input)
    })();
    let (status, value) = match result {
        Ok(value) => (200, value),
        Err(error) => {
            let status = if error.downcast_ref::<SaveConflict>().is_some() {
                409
            } else {
                400
            };
            (status, json!({"error": format!("{error:#}")}))
        }
    };
    respond(
        request,
        status,
        "application/json",
        serde_json::to_vec(&value).expect("JSON serializes"),
    );
}

fn read_input(request: &mut Request) -> Result<Input> {
    if request
        .body_length()
        .is_some_and(|length| length as u64 > MAX_REQUEST)
    {
        bail!("request too large");
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_REQUEST {
        bail!("request too large");
    }
    serde_json::from_slice(&body).context("invalid request")
}

fn serve_asset(request: Request, path: &str) {
    if request.method() != &Method::Get {
        respond(request, 405, "text/plain", b"Method not allowed".to_vec());
        return;
    }
    if let Some(asset) = embedded_asset(path) {
        let response = Response::new(
            StatusCode(200),
            Vec::new(),
            Cursor::new(asset.bytes),
            Some(asset.bytes.len()),
            None,
        );
        send_response(
            request,
            asset.content_type,
            asset_cache_control(path),
            response,
        );
    } else {
        respond(request, 404, "text/plain", b"Not found".to_vec());
    }
}

fn respond(request: Request, status: u16, mime: &str, data: Vec<u8>) {
    let response = Response::from_data(data).with_status_code(StatusCode(status));
    send_response(request, mime, "no-store", response);
}

fn send_response<R: Read>(request: Request, mime: &str, cache: &str, mut response: Response<R>) {
    for (name, value) in [
        ("Content-Type", mime),
        ("Cache-Control", cache),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        (
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self'; frame-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        response.add_header(Header::from_bytes(name, value).expect("static HTTP header"));
    }
    let _ = request.respond(response);
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    identity: Option<Value>,
    column: Option<String>,
    filter: Option<String>,
    view: Option<String>,
    path: Option<String>,
    revision: Option<String>,
    previous_path: Option<String>,
    table: Option<String>,
    after: Option<String>,
    offset: Option<usize>,
    content: Option<String>,
    expected: Option<String>,
    name: Option<String>,
    action: Option<String>,
    state_token: Option<String>,
}

struct Viewer {
    root: PathBuf,
    session: RepositorySession,
    historical_summaries: RefCell<VecDeque<(String, Value)>>,
}

impl Viewer {
    fn open(path: &Path) -> Result<Self> {
        let repository = Repository::discover(path)
            .context("open a folder inside an existing Graft repository")?;
        let root = repository.worktree().canonicalize()?;
        let session = RepositorySession::new(&root);
        session.open()?;
        Ok(Self {
            root,
            session,
            historical_summaries: RefCell::new(VecDeque::new()),
        })
    }

    fn dispatch(&self, endpoint: &str, input: Input) -> Result<Value> {
        match endpoint {
            "/api/snapshot" => self.snapshot(),
            "/api/history" => self.history(&input),
            "/api/commit" => Ok(serde_json::to_value(self.session.commit_changed_paths(
                &CommitChangedPathsOptions {
                    revision: input.revision.context("revision required")?,
                    limit: 100,
                    after: input.after,
                },
            )?)?),
            "/api/file" => self.file(&input),
            "/api/sqlite-diff" => self.sqlite_diff(&input),
            "/api/database" => self.database(&input),
            "/api/database-cell" => self.database_cell(&input),
            "/api/database-tables" => self.database_tables(&input),
            "/api/save" => self.save(&input),
            "/api/branches" => self.branch_command(&input),
            "/api/merge" => self.merge_paths(&input),
            "/api/conflict" => self.conflict(&input),
            "/api/resolve-row" | "/api/resolve-cell" => {
                let path = input.path.context("path required")?;
                self.safe_path(&path)?;
                let table = input.table.context("table required")?;
                let identity = input.identity.context("row identity required")?;
                let expected_state_token = input.state_token.context("merge state required")?;
                let result = match input.action.as_deref() {
                    Some("ours") => MergePathResult::Ours,
                    Some("theirs") => MergePathResult::Theirs,
                    _ => bail!("select current or incoming"),
                };
                let outcome = if endpoint == "/api/resolve-cell" {
                    self.session.resolve_merge_cell(&ResolveMergeCellOptions {
                        path: path.into(),
                        table,
                        identity,
                        column: input.column.context("column required")?,
                        result,
                        expected_state_token,
                    })?
                } else {
                    self.session.resolve_merge_row(&ResolveMergeRowOptions {
                        path: path.into(),
                        table,
                        identity,
                        result,
                        expected_state_token,
                    })?
                };
                Ok(serde_json::to_value(outcome)?)
            }
            "/api/resolve" => {
                let path = input.path.context("path required")?;
                self.safe_path(&path)?;
                if input.action.as_deref() == Some("edited") {
                    let content = input.content.context("resolved content required")?;
                    if content.len() as u64 > MAX_TEXT || content.contains('\0') {
                        bail!("invalid text content");
                    }
                    if content.lines().any(|line| line.starts_with("<<<<<<< "))
                        && content.lines().any(|line| line.starts_with(">>>>>>> "))
                    {
                        bail!("resolve all text conflict blocks before saving");
                    }
                    let current = self.current_content(&path)?;
                    if current["etag"].as_str() != input.expected.as_deref() {
                        bail!(
                            "The merge result changed outside the viewer. Reopen this conflict before saving."
                        );
                    }
                    return Ok(serde_json::to_value(
                        self.session.write_and_stage_text_result(
                            &WriteAndStageTextResultOptions {
                                path: path.into(),
                                content,
                                expected_state_token: input
                                    .state_token
                                    .context("merge state required")?,
                            },
                        )?,
                    )?);
                }
                let result = match input.action.as_deref() {
                    Some("ours") => MergePathResult::Ours,
                    Some("theirs") => MergePathResult::Theirs,
                    _ => bail!("choose ours or theirs"),
                };
                Ok(serde_json::to_value(self.session.set_merge_path_result(
                    &SetMergePathResultOptions {
                        path: path.into(),
                        result,
                        expected_state_token: input.state_token.context("merge state required")?,
                    },
                )?)?)
            }
            "/api/merge-continue" => Ok(serde_json::to_value(
                self.session.continue_merge(&ContinueMergeOptions {
                    message: input
                        .content
                        .filter(|message| !message.trim().is_empty())
                        .context("commit message required")?,
                    expected_state_token: input.state_token.context("merge state required")?,
                })?,
            )?),
            _ => bail!("unknown endpoint"),
        }
    }

    fn history(&self, input: &Input) -> Result<Value> {
        let filter = input.filter.as_deref().unwrap_or("").trim().to_lowercase();
        if filter.is_empty() {
            return Ok(serde_json::to_value(
                self.session.history_summaries(40, input.after.as_deref())?,
            )?);
        }
        if filter.len() > 1024 {
            bail!("filter too long");
        }
        let mut cursor = input.after.clone();
        let mut matches = Vec::new();
        loop {
            let page = self.session.history_summaries(100, cursor.as_deref())?;
            let count = page.commits.len();
            for (index, commit) in page.commits.into_iter().enumerate() {
                if !commit.message.to_lowercase().contains(&filter)
                    && !commit.id.to_lowercase().contains(&filter)
                {
                    continue;
                }
                let id = commit.id.clone();
                matches.push(commit);
                if matches.len() == 40 {
                    return Ok(
                        json!({"commits":matches,"has_more":index + 1 < count || page.has_more,"next_cursor":id}),
                    );
                }
            }
            if !page.has_more {
                return Ok(json!({"commits":matches,"has_more":false,"next_cursor":null}));
            }
            let next = page.next_cursor.context("missing history cursor")?;
            if cursor.as_ref() == Some(&next) {
                bail!("history cursor did not advance");
            }
            cursor = Some(next);
        }
    }

    fn branch_command(&self, input: &Input) -> Result<Value> {
        let command = match input.action.as_deref() {
            None | Some("list") => RepositoryCommand::parse("json_branch", None)?,
            Some(action @ ("create" | "switch")) => {
                let name = input.name.as_deref().context("branch name required")?;
                if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_whitespace)
                {
                    bail!("invalid branch name");
                }
                RepositoryCommand::parse(
                    if action == "create" {
                        "json_branch_create"
                    } else {
                        "json_switch_branch"
                    },
                    Some(name),
                )?
            }
            _ => bail!("unknown branch action"),
        };
        // Branch commands use the canonical service. Release the resident runtime
        // before opening it, then restore the viewer even if the command fails.
        self.session.close()?;
        let result = (|| -> Result<Value> {
            let mut service = RepositoryCommandService::open(self.session.target())?;
            let output = service
                .execute(command)?
                .context("missing branch response")?;
            Ok(serde_json::from_str(&output)?)
        })();
        self.session.open()?;
        result
    }

    fn merge_paths(&self, input: &Input) -> Result<Value> {
        let status = self.session.get_merge_status()?;
        if let MergeStatus::Merging { ref state_token, .. } = status {
            let page = self.session.list_merge_paths(&ListMergePathsOptions {
                filter: MergePathFilter::All,
                limit: 100,
                after: input.after.clone(),
                expected_state_token: state_token.clone(),
            })?;
            Ok(json!({ "status": status, "paths": page.items, "next_cursor": page.next_cursor }))
        } else {
            Ok(json!({ "status": status, "paths": [] }))
        }
    }

    fn conflict(&self, input: &Input) -> Result<Value> {
        let path = input.path.as_deref().context("path required")?;
        self.safe_path(path)?;
        let token = input
            .state_token
            .as_deref()
            .context("merge state required")?;
        let mut after = None;
        let sqlite = loop {
            let page = self.session.list_merge_paths(&ListMergePathsOptions {
                filter: MergePathFilter::All,
                limit: 100,
                after,
                expected_state_token: token.into(),
            })?;
            if let Some(item) = page.items.iter().find(|item| item.path == path) {
                break item.kind == graft::repo::RepoTrackedPathKind::SqliteDatabase;
            }
            after = page.next_cursor;
            if after.is_none() {
                bail!("path is not part of the active merge");
            }
        };
        let read = |version| -> Result<Value> {
            if sqlite {
                return Ok(json!({"content": {"state": "sqlite"}}));
            }
            Ok(serde_json::to_value(self.session.read_merge_version(
                &ReadMergeVersionOptions {
                    path: path.into(),
                    version,
                    max_bytes: MAX_TEXT,
                    expected_state_token: token.into(),
                },
            )?)?)
        };
        let conflicts = self
            .session
            .list_merge_conflicts(&ListMergeConflictsOptions {
                path: path.into(),
                limit: 100,
                after: input.after.clone(),
                expected_state_token: token.into(),
            })?;
        let mut tables = Vec::new();
        if sqlite {
            let connection = Connection::open_with_flags(
                self.safe_path(path)?,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            connection.busy_timeout(Duration::from_millis(500))?;
            let connection = connection.unchecked_transaction()?;
            let names = conflicts
                .items
                .iter()
                .filter_map(|item| item["table"].as_str())
                .collect::<BTreeSet<_>>();
            for name in names {
                let quoted = name.replace('"', "\"\"");
                // Structural conflicts may reference a table absent from the current schema.
                let Ok(statement) =
                    connection.prepare(&format!("SELECT * FROM \"{quoted}\" LIMIT 0"))
                else {
                    continue;
                };
                let columns = statement
                    .column_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let mut info = connection
                    .prepare("SELECT name, type, pk FROM pragma_table_info(?1) WHERE pk > 0")?;
                let keys = info
                    .query_map([name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let has_primary_index: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_index_list(?1) WHERE origin = 'pk')",
                    [name],
                    |row| row.get(0),
                )?;
                let alias = if !has_primary_index
                    && keys.len() == 1
                    && keys[0].1.eq_ignore_ascii_case("INTEGER")
                {
                    Some(keys[0].0.clone())
                } else {
                    None
                };
                tables.push(json!({"name": name, "columns": columns, "rowid_alias": alias}));
            }
        }
        Ok(
            json!({"base": read(MergeVersion::Base)?, "ours": read(MergeVersion::Ours)?, "theirs": read(MergeVersion::Theirs)?, "result": read(MergeVersion::Result)?, "result_etag": if sqlite { None } else { self.current_content(path)?["etag"].as_str().map(str::to_string) }, "tables": tables, "conflicts": conflicts.items, "next_cursor": conflicts.next_cursor}),
        )
    }

    fn snapshot(&self) -> Result<Value> {
        let status = self.session.status()?;
        let mut paths = BTreeSet::new();
        let mut truncated = false;
        'inventory: for kind in [InventoryKind::Tracked, InventoryKind::Untracked] {
            let mut after = None;
            loop {
                let page =
                    self.session
                        .inventory(&InventoryOptions { kind, limit: 1000, after })?;
                for item in page.items {
                    if self.safe_path(&item.path).is_ok_and(|path| path.is_file()) {
                        if paths.len() == MAX_TREE_PATHS {
                            truncated = true;
                            break 'inventory;
                        }
                        paths.insert(item.path);
                    }
                }
                if !page.has_more {
                    break;
                }
                if paths.len() >= MAX_TREE_PATHS {
                    truncated = true;
                    break;
                }
                after = page.next_cursor;
            }
        }
        Ok(json!({
            "root": self.root.file_name().unwrap_or_default().to_string_lossy(),
            "databases": paths.iter().filter(|path| {
                let extension = Path::new(path).extension().and_then(|value| value.to_str()).unwrap_or_default();
                matches!(extension.to_ascii_lowercase().as_str(), "eidos" | "sqlite" | "sqlite3" | "db" | "db3")
                    && self.safe_path(path).is_ok_and(|path| has_sqlite_header(&path))
            }).collect::<Vec<_>>(),
            "paths": paths, "status": status, "truncated": truncated,
            "merge": self.session.get_merge_status()?,
        }))
    }

    fn safe_path(&self, path: &str) -> Result<PathBuf> {
        if path.is_empty() {
            bail!("path required");
        }
        graft::repo::validate_repo_path_identity(path)?;
        if Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
            || path
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".graft"))
        {
            bail!("invalid worktree path");
        }
        let candidate = self.root.join(path);
        // Validate the closest existing ancestor too, including dangling links.
        let mut ancestor = candidate.as_path();
        loop {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => {
                    let resolved = ancestor.canonicalize()?;
                    if !resolved.starts_with(&self.root)
                        || resolved.strip_prefix(&self.root)?.components().any(|part| {
                            part.as_os_str()
                                .to_str()
                                .is_some_and(|part| part.eq_ignore_ascii_case(".graft"))
                        })
                    {
                        bail!("path escapes the worktree");
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().context("invalid path")?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(candidate)
    }

    fn current_content(&self, path: &str) -> Result<Value> {
        let path = self.safe_path(path)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"state":"absent"}));
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            bail!("not a regular file");
        }
        let mut magic = Vec::with_capacity(16);
        fs::File::open(&path)?.take(16).read_to_end(&mut magic)?;
        if magic == b"SQLite format 3\0" {
            return Ok(json!({"state":"sqlite", "size":metadata.len()}));
        }
        if metadata.len() > MAX_TEXT {
            return Ok(json!({"state":"too_large", "size":metadata.len()}));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(path)?
            .take(MAX_TEXT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_TEXT {
            return Ok(json!({"state":"too_large", "size":bytes.len()}));
        }
        let etag = fingerprint(&bytes);
        if bytes.starts_with(b"SQLite format 3\0") {
            return Ok(json!({"state":"sqlite", "size":bytes.len()}));
        }
        if bytes.contains(&0) {
            return Ok(json!({"state":"binary", "size":bytes.len()}));
        }
        match String::from_utf8(bytes) {
            Ok(content) => Ok(json!({"state":"utf8", "content":content, "etag":etag})),
            Err(error) => Ok(json!({"state":"invalid_utf8", "size":error.as_bytes().len()})),
        }
    }

    fn revision_content(&self, path: &str, revision: Option<&str>) -> Result<Value> {
        self.safe_path(path)?;
        let Some(revision) = revision else {
            return Ok(json!({"state":"absent"}));
        };
        let content = self
            .session
            .read_path_content(&ReadPathContentOptions {
                path: PathBuf::from(path),
                revision: revision.to_string(),
                max_bytes: MAX_TEXT,
            })?
            .content;
        match content {
            RepoPathContentState::Utf8 { ref content, .. } if content.contains('\0') => {
                Ok(json!({"state":"binary"}))
            }
            _ => Ok(serde_json::to_value(content)?),
        }
    }

    fn file(&self, input: &Input) -> Result<Value> {
        let path = input.path.as_deref().context("path required")?;
        if input.view.as_deref() == Some("file") {
            return Ok(json!({"before":{"state":"absent"}, "after":self.current_content(path)?}));
        }
        if let Some(revision) = &input.revision {
            let page = self
                .session
                .commit_changed_paths(&CommitChangedPathsOptions {
                    revision: revision.clone(),
                    limit: 1,
                    after: None,
                })?;
            Ok(
                json!({"before":self.revision_content(input.previous_path.as_deref().unwrap_or(path), page.parent.as_deref())?,
                "after":self.revision_content(path, Some(revision))?}),
            )
        } else {
            let status = self.session.status()?;
            let head = status["current_head"].as_str();
            Ok(
                json!({"before":self.revision_content(input.previous_path.as_deref().unwrap_or(path), head)?,
                "after":self.current_content(path)?}),
            )
        }
    }

    fn sqlite_diff(&self, input: &Input) -> Result<Value> {
        self.sqlite_diff_response(input, true)
    }

    fn sqlite_diff_response(&self, input: &Input, include_rows: bool) -> Result<Value> {
        let path = input.path.as_deref().context("path required")?;
        self.safe_path(path)?;
        let (root, from, to) = if let Some(revision) = &input.revision {
            let page = self
                .session
                .commit_changed_paths(&CommitChangedPathsOptions {
                    revision: revision.clone(),
                    limit: 1,
                    after: None,
                })?;
            if page.parent.is_some() {
                (None, page.parent, Some(page.revision))
            } else {
                (Some(page.revision), None, None)
            }
        } else {
            (
                None,
                self.session.status()?["current_head"]
                    .as_str()
                    .map(str::to_string),
                None,
            )
        };
        let unborn = root.is_none() && from.is_none() && to.is_none();
        let staged = unborn
            && self.session.status()?["paths"]
                .as_array()
                .is_some_and(|paths| {
                    paths.iter().any(|entry| {
                        entry["path"] == path
                            && entry["unstaged_change"].is_null()
                            && !entry["staged_change"].is_null()
                    })
                });
        let mut options = SqliteDiffPathsOptions {
            paths: vec![PathBuf::from(path)],
            staged,
            staged_fallback: unborn,
            root,
            from,
            to,
            response: SqliteDiffResponse::Summary,
            limit: 1,
            after: None,
        };
        // Only immutable commit pairs are cached. Worktree reads must continue
        // to observe concurrent agent writes and staged changes.
        let cache_key = input
            .revision
            .as_ref()
            .map(|_| json!([path, options.root, options.from, options.to]).to_string());
        let cached = cache_key.as_ref().and_then(|key| {
            self.historical_summaries
                .borrow()
                .iter()
                .find(|(stored, _)| stored == key)
                .map(|(_, value)| value.clone())
        });
        let file = if let Some(file) = cached {
            file
        } else {
            let summary = self.session.diff_sqlite_paths(&options)?;
            let file = summary.paths.first().context("no diff result")?.diff["files"][0].clone();
            if let Some(key) = cache_key {
                let mut cache = self.historical_summaries.borrow_mut();
                if cache.len() >= 16 {
                    cache.pop_front();
                }
                cache.push_back((key, file.clone()));
            }
            file
        };
        if !include_rows {
            return Ok(file);
        }
        let table = input
            .table
            .clone()
            .or_else(|| file["summaries"][0]["name"].as_str().map(str::to_string));
        let Some(table) = table else {
            let mut result = if file.is_object() { file } else { json!({}) };
            result["summaries"] = json!([]);
            result["tables"] = json!([]);
            label_unborn_diff(&mut result, unborn);
            return Ok(result);
        };
        options.response = SqliteDiffResponse::Rows {
            table,
            limit: PAGE_SIZE,
            after: input.after.clone(),
        };
        let rows = self.session.diff_sqlite_paths(&options)?;
        let mut result = rows.paths.first().context("no row result")?.diff["files"][0].clone();
        result["summaries"] = file["summaries"].clone();
        result["schema_changes"] = file["schema_changes"].clone();
        result["opaque_changes"] = file["opaque_changes"].clone();
        result["limitations"] = file["limitations"].clone();
        label_unborn_diff(&mut result, unborn);
        Ok(result)
    }

    fn database(&self, input: &Input) -> Result<Value> {
        let path = self.safe_path(input.path.as_deref().context("path required")?)?;
        let version = database_version(&path);
        let version_key = version.to_string();
        if input.offset.is_some() {
            let expected = input
                .expected
                .as_deref()
                .context("database version required for row pages")?;
            if expected != version_key.as_str() {
                bail!("The database changed. Refresh the table before loading more rows.");
            }
        } else if input.expected.as_deref() == Some(version_key.as_str()) {
            return Ok(json!({"unchanged":true}));
        }
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_millis(500))?;
        let connection = connection.unchecked_transaction()?;
        let tables = connection.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let table = input.table.as_ref().or(tables.first());
        let Some(table) = table else {
            return Ok(
                json!({"tables":tables,"columns":[],"rows":[],"total_rows":0,"offset":0,"has_more":false,"version":version}),
            );
        };
        if !tables.contains(table) {
            bail!("unknown table");
        }
        let offset = input.offset.unwrap_or(0);
        if offset > 1_000_000_000 {
            bail!("offset too large");
        }
        let quoted = table.replace('"', "\"\"");
        let total_rows: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                row.get(0)
            })?;
        let mut statement =
            connection.prepare(&format!("SELECT * FROM \"{quoted}\" LIMIT ?1 OFFSET ?2"))?;
        let columns = statement
            .column_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        let mut query = statement.query([(PAGE_SIZE + 1) as i64, offset as i64])?;
        let mut rows = Vec::new();
        let mut truncated_cells = Vec::new();
        while let Some(row) = query.next()? {
            let mut values = Vec::with_capacity(columns.len());
            let mut truncated = Vec::new();
            for index in 0..columns.len() {
                let value = row.get_ref(index)?;
                if let ValueRef::Text(text) = value
                    && String::from_utf8_lossy(text).chars().count() > 4096
                {
                    truncated.push(index);
                }
                values.push(sqlite_value(value));
            }
            rows.push(values);
            truncated_cells.push(truncated);
        }
        let has_more = rows.len() > PAGE_SIZE;
        rows.truncate(PAGE_SIZE);
        truncated_cells.truncate(PAGE_SIZE);
        if database_version(&path) != version {
            bail!("The database changed while reading. Refresh the table and try again.");
        }
        Ok(
            json!({"tables":tables,"table":table,"columns":columns,"rows":rows,"truncated_cells":truncated_cells,"has_more":has_more,"offset":offset,"total_rows":total_rows,"version":version}),
        )
    }

    fn database_cell(&self, input: &Input) -> Result<Value> {
        let path = self.safe_path(input.path.as_deref().context("path required")?)?;
        let expected = input
            .expected
            .as_deref()
            .context("database version required")?;
        let expected: Value = serde_json::from_str(expected).context("invalid database version")?;
        if database_version(&path) != expected {
            bail!("The database changed. Close this detail and reopen the cell after refresh.");
        }
        let table = input
            .table
            .as_deref()
            .context("table required")?
            .replace('"', "\"\"");
        let column = input.column.as_deref().context("column required")?;
        let offset = input.offset.context("row offset required")?;
        if offset > 1_000_000_000 {
            bail!("offset too large");
        }
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_millis(500))?;
        let transaction = connection.unchecked_transaction()?;
        let statement = transaction.prepare(&format!("SELECT * FROM \"{table}\" LIMIT 0"))?;
        if !statement.column_names().contains(&column) {
            bail!("unknown column");
        }
        drop(statement);
        let column = column.replace('"', "\"\"");
        let value = transaction
            .query_row(
                &format!("SELECT \"{column}\" FROM \"{table}\" LIMIT 1 OFFSET ?1"),
                [offset as i64],
                |row| {
                    let value = row.get_ref(0)?;
                    Ok(match value {
                        ValueRef::Text(text) if text.len() as u64 <= MAX_TEXT => {
                            Some(json!(String::from_utf8_lossy(text)))
                        }
                        ValueRef::Text(_) => None,
                        _ => Some(sqlite_value(value)),
                    })
                },
            )?
            .context("This value exceeds the 2 MB detail limit; use the CLI to inspect it.")?;
        if database_version(&path) != expected {
            bail!("The database changed while reading. Reopen the cell after refresh.");
        }
        Ok(json!({"value":value}))
    }

    fn database_tables(&self, input: &Input) -> Result<Value> {
        if input.view.as_deref() == Some("file") {
            let path = self.safe_path(input.path.as_deref().context("path required")?)?;
            let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.busy_timeout(Duration::from_millis(500))?;
            let tables = connection.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
                .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(
                json!({"tables": tables.into_iter().map(|name| json!({"name":name})).collect::<Vec<_>>() }),
            );
        }
        Ok(diff_table_catalog(
            &self.sqlite_diff_response(input, false)?,
        ))
    }

    fn save(&self, input: &Input) -> Result<Value> {
        let path = input.path.as_deref().context("path required")?;
        let current = self.current_content(path)?;
        if current["state"] != "utf8" {
            bail!("only existing UTF-8 text files can be edited");
        }
        let expected = input
            .expected
            .as_deref()
            .context("expected file version required")?;
        if current["etag"].as_str() != Some(expected) {
            return Err(SaveConflict.into());
        }
        let content = input.content.as_deref().context("content required")?;
        if content.len() as u64 > MAX_TEXT || content.contains('\0') {
            bail!("invalid text content");
        }
        let full_path = self.safe_path(path)?;
        if fs::symlink_metadata(&full_path)?.file_type().is_symlink() {
            bail!("editing symbolic links is not supported");
        }
        let mut file =
            tempfile::NamedTempFile::new_in(full_path.parent().context("missing parent")?)?;
        file.as_file()
            .set_permissions(fs::metadata(&full_path)?.permissions())?;
        file.write_all(content.as_bytes())?;
        file.as_file().sync_all()?;
        // Recheck after preparing the replacement to catch concurrent agent saves.
        if self.current_content(path)?["etag"].as_str() != Some(expected) {
            return Err(SaveConflict.into());
        }
        self.safe_path(path)?;
        file.persist(&full_path).context("could not save file")?;
        Ok(json!({"etag":fingerprint(content.as_bytes())}))
    }
}

fn diff_table_catalog(diff: &Value) -> Value {
    let mut tables = BTreeMap::new();
    if let Some(summaries) = diff["summaries"].as_array() {
        for summary in summaries {
            if let Some(name) = summary["name"].as_str() {
                tables.insert(name.to_string(), summary.clone());
            }
        }
    }
    if let Some(changes) = diff["schema_changes"].as_array() {
        for change in changes {
            if change["entry_type"] != "table" {
                continue;
            }
            if let Some(name) = change["name"].as_str() {
                let table = tables
                    .entry(name.to_string())
                    .or_insert_with(|| json!({"name":name}));
                table["schema_changed"] = json!(true);
                table["change"] = change["op"].clone();
            }
        }
    }
    if let Some(changes) = diff["opaque_changes"].as_array() {
        for change in changes {
            if let Some(name) = change["name"].as_str() {
                let table = tables
                    .entry(name.to_string())
                    .or_insert_with(|| json!({"name":name}));
                table["opaque"] = json!(true);
                table["change"] = change["change"].clone();
            }
        }
    }
    let mut catalog = json!({"tables": tables.into_values().collect::<Vec<_>>()});
    if let Some(name) = diff["summaries"][0]["name"].as_str() {
        catalog["default_table"] = json!(name);
    }
    catalog
}

fn has_sqlite_header(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = Vec::with_capacity(16);
    file.take(16).read_to_end(&mut magic).is_ok() && magic == b"SQLite format 3\0"
}

fn label_unborn_diff(result: &mut Value, unborn: bool) {
    if unborn {
        result["message"] = json!("Before the first commit, diff uses the staging area.");
    }
}

// WAL writes invalidate cached pages even before checkpointing the main file.
fn database_version(path: &Path) -> Value {
    let stamp = |file: &Path| {
        fs::metadata(file).ok().map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos().to_string());
            json!([metadata.len(), modified])
        })
    };
    let wal = path.with_file_name(format!(
        "{}-wal",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    json!([stamp(path), stamp(&wal)])
}

fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => {
            let text = String::from_utf8_lossy(value);
            if text.chars().count() > 4096 {
                json!(format!("{}…", text.chars().take(4096).collect::<String>()))
            } else {
                json!(text)
            }
        }
        ValueRef::Blob(value) => json!({"blob_bytes":value.len()}),
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug)]
struct SaveConflict;
impl std::fmt::Display for SaveConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("This file changed outside the viewer. Your draft is still available; reopen the file before saving.")
    }
}
impl std::error::Error for SaveConflict {}

#[cfg(test)]
mod tests;
