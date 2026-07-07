use super::*;
use graft::repo::{RepoConflictChange, RepoStagedChange, RepoStatusCounts, RepoWorktreeChange};

#[test]
fn legacy_volume_pragmas_are_debug_only() {
    let legacy = Pragma { name: "graft_volume_push", arg: None };
    assert!(GraftPragma::try_from(&legacy).is_err());

    let debug = Pragma {
        name: "graft_debug_volume_push",
        arg: None,
    };
    assert!(matches!(
        GraftPragma::try_from(&debug).unwrap(),
        GraftPragma::VolumePush
    ));
}

#[test]
fn undocumented_repo_compat_pragmas_are_rejected() {
    for name in [
        "graft_repo_status",
        "graft_remove",
        "graft_branch_move",
        "graft_branch_set_upstream",
        "graft_branch_set_upstream_to",
        "graft_tag_rm",
        "graft_remote_rm",
        "graft_remote_mv",
    ] {
        let pragma = Pragma { name, arg: Some("app.db") };
        assert!(
            GraftPragma::try_from(&pragma).is_err(),
            "{name} should be rejected"
        );
    }

    let status = Pragma { name: "graft_status", arg: None };
    assert!(matches!(
        GraftPragma::try_from(&status).unwrap(),
        GraftPragma::Status { spec: StatusSpec { kind: None } }
    ));
    let json_status = Pragma {
        name: "graft_json_status",
        arg: Some("--kind sqlite"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_status).unwrap(),
        GraftPragma::JsonStatus {
            spec: StatusSpec {
                kind: Some(RepoTrackedPathKind::SqliteDatabase),
            },
        }
    ));

    let json_init = Pragma { name: "graft_json_init", arg: None };
    assert!(matches!(
        GraftPragma::try_from(&json_init).unwrap(),
        GraftPragma::JsonRepoInit
    ));

    let remove = Pragma { name: "graft_rm", arg: Some("app.db") };
    assert!(matches!(
        GraftPragma::try_from(&remove).unwrap(),
        GraftPragma::Remove { .. }
    ));

    let json_remove = Pragma {
        name: "graft_json_rm",
        arg: Some("app.db"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_remove).unwrap(),
        GraftPragma::JsonRemove { .. }
    ));

    let json_commit = Pragma {
        name: "graft_json_commit",
        arg: Some("message"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_commit).unwrap(),
        GraftPragma::JsonCommit { .. }
    ));
}

#[test]
fn json_log_status_mode_is_opt_in() {
    let legacy = Pragma { name: "graft_json_log", arg: None };
    assert!(matches!(
        GraftPragma::try_from(&legacy).unwrap(),
        GraftPragma::JsonLog { mode: JsonLogMode::LegacyArray }
    ));

    let with_status = Pragma {
        name: "graft_json_log",
        arg: Some("--with-status"),
    };
    assert!(matches!(
        GraftPragma::try_from(&with_status).unwrap(),
        GraftPragma::JsonLog { mode: JsonLogMode::WithStatus }
    ));

    let invalid = Pragma {
        name: "graft_json_log",
        arg: Some("--status"),
    };
    assert!(GraftPragma::try_from(&invalid).is_err());
}

#[test]
fn json_config_list_status_mode_is_opt_in() {
    let legacy = Pragma {
        name: "graft_json_config_list",
        arg: None,
    };
    assert!(matches!(
        GraftPragma::try_from(&legacy).unwrap(),
        GraftPragma::JsonConfigList { mode: JsonConfigListMode::LegacyArray }
    ));

    let with_status = Pragma {
        name: "graft_json_config_list",
        arg: Some("--with-status"),
    };
    assert!(matches!(
        GraftPragma::try_from(&with_status).unwrap(),
        GraftPragma::JsonConfigList { mode: JsonConfigListMode::WithStatus }
    ));

    let invalid = Pragma {
        name: "graft_json_config_list",
        arg: Some("--status"),
    };
    assert!(GraftPragma::try_from(&invalid).is_err());
}

#[test]
fn json_tags_status_mode_is_opt_in() {
    let legacy = Pragma { name: "graft_json_tags", arg: None };
    assert!(matches!(
        GraftPragma::try_from(&legacy).unwrap(),
        GraftPragma::JsonTags { mode: JsonTagsMode::LegacyArray }
    ));

    let with_status = Pragma {
        name: "graft_json_tags",
        arg: Some("--with-status"),
    };
    assert!(matches!(
        GraftPragma::try_from(&with_status).unwrap(),
        GraftPragma::JsonTags { mode: JsonTagsMode::WithStatus }
    ));

    let invalid = Pragma {
        name: "graft_json_tags",
        arg: Some("--status"),
    };
    assert!(GraftPragma::try_from(&invalid).is_err());
}

#[test]
fn async_job_pragmas_are_parsed() {
    let fetch = Pragma {
        name: "graft_fetch_async",
        arg: Some("--all origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&fetch).unwrap(),
        GraftPragma::FetchAsync { remote: Some(_), all: true, .. }
    ));

    let json_fetch = Pragma {
        name: "graft_json_fetch_async",
        arg: Some("origin main"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_fetch).unwrap(),
        GraftPragma::JsonFetchAsync {
            remote: Some(_),
            branch: Some(_),
            mode: JsonFetchAsyncMode::LegacyId,
            ..
        }
    ));

    let json_fetch_with_status = Pragma {
        name: "graft_json_fetch_async",
        arg: Some("--with-status --all origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_fetch_with_status).unwrap(),
        GraftPragma::JsonFetchAsync {
            remote: Some(_),
            all: true,
            mode: JsonFetchAsyncMode::WithStatus,
            ..
        }
    ));

    let status = Pragma {
        name: "graft_job_status",
        arg: Some("graft-job-1"),
    };
    assert!(matches!(
        GraftPragma::try_from(&status).unwrap(),
        GraftPragma::JobStatus { .. }
    ));

    let json_status = Pragma {
        name: "graft_json_job_status",
        arg: Some("graft-job-1"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_status).unwrap(),
        GraftPragma::JsonJobStatus { .. }
    ));

    let result = Pragma {
        name: "graft_job_result",
        arg: Some("graft-job-1"),
    };
    assert!(matches!(
        GraftPragma::try_from(&result).unwrap(),
        GraftPragma::JobResult { .. }
    ));
}

#[test]
fn parse_remote_add_supports_explicit_s3_endpoint() {
    let (name, config) = parse_remote_add(
        "origin s3_compatible://my-bucket/prod/app?endpoint=http://localhost:9000",
    )
    .unwrap();

    assert_eq!(name, "origin");
    assert_eq!(
        config,
        RemoteConfig::S3Compatible {
            bucket: "my-bucket".to_string(),
            prefix: Some("prod/app".to_string()),
            endpoint: Some("http://localhost:9000".to_string()),
        }
    );
    assert_eq!(
        remote_config_uri(&config),
        "s3://my-bucket/prod/app?endpoint=http://localhost:9000"
    );
}

#[test]
fn parse_remote_add_rejects_unknown_s3_query_parameters() {
    assert!(parse_remote_add("origin s3://my-bucket/prod?region=auto").is_err());
}

#[test]
fn parse_remote_add_supports_graft_http_remote() {
    let (name, config) = parse_remote_add(
        "origin graft+https://graft.example.com/api/graft/v1/repos/acme/app?token_env=GRAFT_TOKEN",
    )
    .unwrap();

    assert_eq!(name, "origin");
    assert_eq!(
        config,
        RemoteConfig::Http {
            url: "https://graft.example.com/api/graft/v1/repos/acme/app".to_string(),
            token_env: Some("GRAFT_TOKEN".to_string()),
        }
    );
    assert_eq!(
        remote_config_uri(&config),
        "graft+https://graft.example.com/api/graft/v1/repos/acme/app?token_env=GRAFT_TOKEN"
    );
}

#[test]
fn parse_remote_add_rejects_unknown_graft_http_query_parameters() {
    assert!(parse_remote_add("origin graft+https://graft.example.com/api?token=secret").is_err());
}

#[test]
fn parse_remote_rename_requires_two_names() {
    assert_eq!(
        parse_remote_rename("origin upstream").unwrap(),
        ("origin".to_string(), "upstream".to_string())
    );
    assert!(parse_remote_rename("origin").is_err());
    assert!(parse_remote_rename("origin upstream backup").is_err());
}

#[test]
fn parse_json_remote_pragmas() {
    let add = Pragma {
        name: "graft_json_remote_add",
        arg: Some("origin memory"),
    };
    assert!(matches!(
        GraftPragma::try_from(&add).unwrap(),
        GraftPragma::JsonRemoteAdd { name, config: RemoteConfig::Memory } if name == "origin"
    ));

    let remove = Pragma {
        name: "graft_json_remote_remove",
        arg: Some("origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&remove).unwrap(),
        GraftPragma::JsonRemoteRemove { name } if name == "origin"
    ));

    let rename = Pragma {
        name: "graft_json_remote_rename",
        arg: Some("origin upstream"),
    };
    assert!(matches!(
        GraftPragma::try_from(&rename).unwrap(),
        GraftPragma::JsonRemoteRename { old, new } if old == "origin" && new == "upstream"
    ));

    let get_url = Pragma {
        name: "graft_json_remote_get_url",
        arg: Some("origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&get_url).unwrap(),
        GraftPragma::JsonRemoteGetUrl { name } if name == "origin"
    ));

    let set_url = Pragma {
        name: "graft_json_remote_set_url",
        arg: Some("origin memory"),
    };
    assert!(matches!(
        GraftPragma::try_from(&set_url).unwrap(),
        GraftPragma::JsonRemoteSetUrl { name, config: RemoteConfig::Memory } if name == "origin"
    ));

    let prune = Pragma {
        name: "graft_json_remote_prune",
        arg: Some("origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&prune).unwrap(),
        GraftPragma::JsonRemotePrune { name } if name == "origin"
    ));

    let ls_remote = Pragma {
        name: "graft_json_ls_remote",
        arg: Some("origin"),
    };
    assert!(matches!(
        GraftPragma::try_from(&ls_remote).unwrap(),
        GraftPragma::JsonLsRemote { name } if name == "origin"
    ));

    let remotes = Pragma { name: "graft_json_remotes", arg: None };
    assert!(matches!(
        GraftPragma::try_from(&remotes).unwrap(),
        GraftPragma::JsonRemotes
    ));
}

#[test]
fn parse_branch_list_mode_supports_remote_and_all() {
    assert_eq!(parse_branch_list_mode(None).unwrap(), BranchListMode::Local);
    assert_eq!(
        parse_branch_list_mode(Some("--remote")).unwrap(),
        BranchListMode::Remote
    );
    assert_eq!(
        parse_branch_list_mode(Some("-r")).unwrap(),
        BranchListMode::Remote
    );
    assert_eq!(
        parse_branch_list_mode(Some("--all")).unwrap(),
        BranchListMode::All
    );
    assert_eq!(
        parse_branch_list_mode(Some("-a")).unwrap(),
        BranchListMode::All
    );
    assert!(parse_branch_list_mode(Some("--remote origin")).is_err());
}

#[test]
fn parse_repo_clone_arg_supports_default_branch_and_branch_flags() {
    assert_eq!(
        parse_repo_clone_arg("fs:///srv/graft/app").unwrap(),
        RepoCloneSpec {
            config: RemoteConfig::Fs { root: "/srv/graft/app".to_string() },
            branch: None,
        }
    );
    assert_eq!(
        parse_repo_clone_arg("fs:///srv/graft/app feature/search").unwrap(),
        RepoCloneSpec {
            config: RemoteConfig::Fs { root: "/srv/graft/app".to_string() },
            branch: Some("feature/search".to_string()),
        }
    );
    assert_eq!(
        parse_repo_clone_arg("--branch feature/search memory").unwrap(),
        RepoCloneSpec {
            config: RemoteConfig::Memory,
            branch: Some("feature/search".to_string()),
        }
    );
    assert_eq!(
        parse_repo_clone_arg("-b release/1.0 memory").unwrap(),
        RepoCloneSpec {
            config: RemoteConfig::Memory,
            branch: Some("release/1.0".to_string()),
        }
    );
    assert!(parse_repo_clone_arg("").is_err());
    assert!(parse_repo_clone_arg("--branch feature/search").is_err());
    assert!(parse_repo_clone_arg("memory feature/search extra").is_err());

    let json_clone = Pragma {
        name: "graft_json_clone",
        arg: Some("--branch feature/search memory"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_clone).unwrap(),
        GraftPragma::JsonRepoClone {
            spec: RepoCloneSpec {
                config: RemoteConfig::Memory,
                branch: Some(branch),
            }
        } if branch == "feature/search"
    ));
}

#[test]
fn parse_remote_branch_arg_supports_all_remote() {
    assert_eq!(
        parse_remote_branch_arg(Some("--all origin")).unwrap(),
        RemoteBranchArg {
            remote: Some("origin".to_string()),
            branch: None,
            refspec: None,
            all: true,
            force: false,
        }
    );
    assert_eq!(
        parse_remote_branch_arg(Some("origin main")).unwrap(),
        RemoteBranchArg {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            refspec: None,
            all: false,
            force: false,
        }
    );
    assert_eq!(
        parse_remote_branch_arg(Some("--force origin main")).unwrap(),
        RemoteBranchArg {
            remote: Some("origin".to_string()),
            branch: Some("main".to_string()),
            refspec: None,
            all: false,
            force: true,
        }
    );
    assert_eq!(
        parse_remote_branch_arg(Some("main:backup")).unwrap(),
        RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: Some("main:backup".to_string()),
            all: false,
            force: false,
        }
    );
    assert_eq!(
        parse_remote_branch_arg(Some("origin refs/heads/*:refs/remotes/origin/review/*")).unwrap(),
        RemoteBranchArg {
            remote: Some("origin".to_string()),
            branch: None,
            refspec: Some("refs/heads/*:refs/remotes/origin/review/*".to_string()),
            all: false,
            force: false,
        }
    );
    assert_eq!(
        parse_remote_branch_arg(Some("--all -f origin")).unwrap(),
        RemoteBranchArg {
            remote: Some("origin".to_string()),
            branch: None,
            refspec: None,
            all: true,
            force: true,
        }
    );
    assert!(parse_remote_branch_arg(Some("--all origin main")).is_err());
}

#[test]
fn parse_debug_diff_lsn_arg_requires_two_log_refs() {
    let log: LogId = "74ggbzxuMf-2uAmM7FwXntwW".parse().unwrap();
    let (from, to) =
        parse_debug_diff_lsn_arg("74ggbzxuMf-2uAmM7FwXntwW:2 74ggbzxuMf-2uAmM7FwXntwW:3").unwrap();

    assert_eq!(from, LogRef::new(log.clone(), LSN::new(2)));
    assert_eq!(to, LogRef::new(log, LSN::new(3)));
    assert!(parse_debug_diff_lsn_arg("74ggbzxuMf-2uAmM7FwXntwW:2").is_err());
    assert!(parse_debug_diff_lsn_arg("2 3").is_err());
}

#[test]
fn parse_repo_diff_arg_supports_row_mode() {
    assert_eq!(
        parse_repo_diff_arg(Some("--rows")).unwrap(),
        RepoDiffSpec {
            mode: DiffMode::Rows,
            kind: None,
            target: RepoDiffTarget::Worktree { path: None },
        }
    );
    assert_eq!(
        parse_repo_diff_arg(Some("--rows --staged -- app.db")).unwrap(),
        RepoDiffSpec {
            mode: DiffMode::Rows,
            kind: None,
            target: RepoDiffTarget::Staged { path: Some("app.db".to_string()) },
        }
    );
    assert_eq!(
        parse_repo_diff_arg(Some("--kind db --staged")).unwrap(),
        RepoDiffSpec {
            mode: DiffMode::Default,
            kind: Some(RepoTrackedPathKind::SqliteDatabase),
            target: RepoDiffTarget::Staged { path: None },
        }
    );
    assert_eq!(
        parse_repo_diff_arg(Some("--rows HEAD~1 HEAD -- app.db")).unwrap(),
        RepoDiffSpec {
            mode: DiffMode::Rows,
            kind: None,
            target: RepoDiffTarget::Revisions {
                from: "HEAD~1".to_string(),
                to: "HEAD".to_string(),
                path: Some("app.db".to_string()),
            },
        }
    );
    assert_eq!(
        parse_repo_diff_arg(Some("HEAD -- --rows")).unwrap(),
        RepoDiffSpec {
            mode: DiffMode::Default,
            kind: None,
            target: RepoDiffTarget::RevisionToWorktree {
                rev: "HEAD".to_string(),
                path: Some("--rows".to_string()),
            },
        }
    );
    assert!(parse_repo_diff_arg(Some("--rows --rows")).is_err());
    assert!(parse_repo_diff_arg(Some("--kind nope")).is_err());
    assert!(parse_repo_diff_arg(Some("--kind db --kind text_file")).is_err());
}

#[test]
fn parse_repo_add_arg_supports_force() {
    assert_eq!(
        parse_repo_add_arg(None).unwrap(),
        RepoAddSpec {
            path: None,
            force: false,
            all: false,
            kind: None,
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("--all")).unwrap(),
        RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: None,
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("-A")).unwrap(),
        RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: None,
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("--all --kind db")).unwrap(),
        RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: Some(RepoTrackedPathKind::SqliteDatabase),
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("--kind binary_file -A")).unwrap(),
        RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: Some(RepoTrackedPathKind::BinaryFile),
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("assets/readme.md")).unwrap(),
        RepoAddSpec {
            path: Some(PathBuf::from("assets/readme.md")),
            force: false,
            all: false,
            kind: None,
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("--force -- assets/readme.md")).unwrap(),
        RepoAddSpec {
            path: Some(PathBuf::from("assets/readme.md")),
            force: true,
            all: false,
            kind: None,
        }
    );
    assert_eq!(
        parse_repo_add_arg(Some("-f assets/readme.md")).unwrap(),
        RepoAddSpec {
            path: Some(PathBuf::from("assets/readme.md")),
            force: true,
            all: false,
            kind: None,
        }
    );
    assert!(parse_repo_add_arg(Some("--force --all")).is_err());
    assert!(parse_repo_add_arg(Some("--kind db")).is_err());
    assert!(parse_repo_add_arg(Some("--all --kind nope")).is_err());
    assert!(parse_repo_add_arg(Some("--force --all --kind db")).is_err());
    assert!(parse_repo_add_arg(Some("--unknown assets/readme.md")).is_err());
}

#[test]
fn parse_repo_remove_arg_supports_cached() {
    assert_eq!(
        parse_repo_remove_arg(None).unwrap(),
        RepoRemoveSpec { path: None, cached: false }
    );
    assert_eq!(
        parse_repo_remove_arg(Some("assets/readme.md")).unwrap(),
        RepoRemoveSpec {
            path: Some(PathBuf::from("assets/readme.md")),
            cached: false,
        }
    );
    assert_eq!(
        parse_repo_remove_arg(Some("--cached")).unwrap(),
        RepoRemoveSpec { path: None, cached: true }
    );
    assert_eq!(
        parse_repo_remove_arg(Some("--cached -- assets/readme.md")).unwrap(),
        RepoRemoveSpec {
            path: Some(PathBuf::from("assets/readme.md")),
            cached: true,
        }
    );
    assert!(parse_repo_remove_arg(Some("--cached --cached")).is_err());
    assert!(parse_repo_remove_arg(Some("--unknown assets/readme.md")).is_err());
}

#[test]
fn parse_repo_audit_arg_supports_repair() {
    assert_eq!(
        parse_repo_audit_arg(None).unwrap(),
        RepoAuditSpec { repair: false, remote: None }
    );
    assert_eq!(
        parse_repo_audit_arg(Some("")).unwrap(),
        RepoAuditSpec { repair: false, remote: None }
    );
    assert_eq!(
        parse_repo_audit_arg(Some("--repair")).unwrap(),
        RepoAuditSpec { repair: true, remote: None }
    );
    assert_eq!(
        parse_repo_audit_arg(Some("--repair origin")).unwrap(),
        RepoAuditSpec {
            repair: true,
            remote: Some("origin".to_string()),
        }
    );
    assert!(parse_repo_audit_arg(Some("origin")).is_err());
    assert!(parse_repo_audit_arg(Some("--repair --repair")).is_err());
    assert!(parse_repo_audit_arg(Some("--unknown")).is_err());
}

#[test]
fn parse_lfs_fetch_arg_supports_remote_and_revision() {
    assert_eq!(
        parse_lfs_fetch_arg(None).unwrap(),
        LargeFileFetchSpec { remote: None, rev: None }
    );
    assert_eq!(
        parse_lfs_fetch_arg(Some("")).unwrap(),
        LargeFileFetchSpec { remote: None, rev: None }
    );
    assert_eq!(
        parse_lfs_fetch_arg(Some("HEAD~1")).unwrap(),
        LargeFileFetchSpec {
            remote: None,
            rev: Some("HEAD~1".to_string())
        }
    );
    assert_eq!(
        parse_lfs_fetch_arg(Some("--remote origin origin/main")).unwrap(),
        LargeFileFetchSpec {
            remote: Some("origin".to_string()),
            rev: Some("origin/main".to_string())
        }
    );
    assert!(parse_lfs_fetch_arg(Some("--remote")).is_err());
    assert!(parse_lfs_fetch_arg(Some("--remote origin --remote upstream")).is_err());
    assert!(parse_lfs_fetch_arg(Some("HEAD main")).is_err());
    assert!(parse_lfs_fetch_arg(Some("--unknown")).is_err());
}

#[test]
fn parse_lfs_status_arg_supports_optional_revision() {
    assert_eq!(
        parse_lfs_status_arg(None).unwrap(),
        LargeFileStatusSpec { rev: None }
    );
    assert_eq!(
        parse_lfs_status_arg(Some("")).unwrap(),
        LargeFileStatusSpec { rev: None }
    );
    assert_eq!(
        parse_lfs_status_arg(Some("HEAD~1")).unwrap(),
        LargeFileStatusSpec { rev: Some("HEAD~1".to_string()) }
    );
    assert!(parse_lfs_status_arg(Some("HEAD main")).is_err());
    assert!(parse_lfs_status_arg(Some("--unknown")).is_err());
}

#[test]
fn parse_lfs_prune_arg_defaults_to_dry_run() {
    assert_eq!(
        parse_lfs_prune_arg(None).unwrap(),
        LargeFilePruneSpec { dry_run: true }
    );
    assert_eq!(
        parse_lfs_prune_arg(Some("")).unwrap(),
        LargeFilePruneSpec { dry_run: true }
    );
    assert_eq!(
        parse_lfs_prune_arg(Some("--dry-run")).unwrap(),
        LargeFilePruneSpec { dry_run: true }
    );
    assert_eq!(
        parse_lfs_prune_arg(Some("--force")).unwrap(),
        LargeFilePruneSpec { dry_run: false }
    );
    assert!(parse_lfs_prune_arg(Some("--force --dry-run")).is_err());
    assert!(parse_lfs_prune_arg(Some("--unknown")).is_err());
}

#[test]
fn payload_pragmas_alias_lfs_payload_pragmas() {
    assert!(matches!(
        GraftPragma::try_from(&Pragma {
            name: "graft_payload_fetch",
            arg: Some("--remote origin HEAD")
        })
        .unwrap(),
        GraftPragma::LargeFileFetch { .. }
    ));
    assert!(matches!(
        GraftPragma::try_from(&Pragma {
            name: "graft_json_payload_status",
            arg: Some("HEAD")
        })
        .unwrap(),
        GraftPragma::JsonLargeFileStatus { .. }
    ));
    assert!(matches!(
        GraftPragma::try_from(&Pragma {
            name: "graft_payload_prune",
            arg: Some("--dry-run")
        })
        .unwrap(),
        GraftPragma::LargeFilePrune { .. }
    ));
}

#[test]
fn parse_ls_files_arg_supports_stage() {
    assert_eq!(
        parse_ls_files_arg(None).unwrap(),
        LsFilesSpec {
            stage: false,
            details: false,
            others: false,
            kind: None
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("")).unwrap(),
        LsFilesSpec {
            stage: false,
            details: false,
            others: false,
            kind: None
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("--stage")).unwrap(),
        LsFilesSpec {
            stage: true,
            details: false,
            others: false,
            kind: None
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("-s")).unwrap(),
        LsFilesSpec {
            stage: true,
            details: false,
            others: false,
            kind: None
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("--kind sqlite")).unwrap(),
        LsFilesSpec {
            stage: false,
            details: false,
            others: false,
            kind: Some(RepoTrackedPathKind::SqliteDatabase),
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("--stage --kind binary_file")).unwrap(),
        LsFilesSpec {
            stage: true,
            details: false,
            others: false,
            kind: Some(RepoTrackedPathKind::BinaryFile),
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("--details --kind text")).unwrap(),
        LsFilesSpec {
            stage: false,
            details: true,
            others: false,
            kind: Some(RepoTrackedPathKind::TextFile),
        }
    );
    assert_eq!(
        parse_ls_files_arg(Some("--others --kind binary")).unwrap(),
        LsFilesSpec {
            stage: false,
            details: false,
            others: true,
            kind: Some(RepoTrackedPathKind::BinaryFile),
        }
    );
    assert!(parse_ls_files_arg(Some("--kind binary -s")).is_ok());
    assert!(parse_ls_files_arg(Some("--unknown")).is_err());
    assert!(parse_ls_files_arg(Some("--kind nope")).is_err());
    assert!(parse_ls_files_arg(Some("--stage --stage")).is_err());
    assert!(parse_ls_files_arg(Some("--details --details")).is_err());
    assert!(parse_ls_files_arg(Some("--others --others")).is_err());
    assert!(parse_ls_files_arg(Some("--stage --details")).is_err());
    assert!(parse_ls_files_arg(Some("--stage --others")).is_err());
    assert!(parse_ls_files_arg(Some("--details --others")).is_err());
}

#[test]
fn parse_status_arg_supports_kind_filter() {
    assert_eq!(parse_status_arg(None).unwrap(), StatusSpec { kind: None });
    assert_eq!(
        parse_status_arg(Some("")).unwrap(),
        StatusSpec { kind: None }
    );
    assert_eq!(
        parse_status_arg(Some("--kind db")).unwrap(),
        StatusSpec {
            kind: Some(RepoTrackedPathKind::SqliteDatabase)
        }
    );
    assert_eq!(
        parse_status_arg(Some("--kind binary_file")).unwrap(),
        StatusSpec {
            kind: Some(RepoTrackedPathKind::BinaryFile)
        }
    );
    assert!(parse_status_arg(Some("--kind nope")).is_err());
    assert!(parse_status_arg(Some("--kind text_file --kind binary_file")).is_err());
    assert!(parse_status_arg(Some("--stage")).is_err());
}

#[test]
fn status_kind_filter_recomputes_paths_and_counts() {
    let status = RepoStatus {
        worktree: PathBuf::from("/repo"),
        graft_dir: PathBuf::from("/repo/.graft"),
        repository_format_version: 1,
        head: Head::branch("main"),
        head_target: Some("head".to_string()),
        merge_head: None,
        orig_head: None,
        dirty: true,
        has_unstaged_changes: true,
        has_staged_changes: true,
        has_conflicts: true,
        work_in_progress: true,
        counts: RepoStatusCounts { unstaged: 2, staged: 1, conflicted: 1 },
        paths: Vec::new(),
        unstaged: vec!["app.db".to_string(), "assets/readme.md".to_string()],
        unstaged_changes: vec![
            RepoWorktreeChange {
                path: "app.db".to_string(),
                change: RepoWorktreeChangeKind::Modified,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
            },
            RepoWorktreeChange {
                path: "assets/readme.md".to_string(),
                change: RepoWorktreeChangeKind::Modified,
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
            },
        ],
        staged: vec!["app.db".to_string()],
        staged_changes: vec![RepoStagedChange {
            path: "app.db".to_string(),
            change: RepoFileChange::Modified,
            kind: RepoTrackedPathKind::SqliteDatabase,
            storage: RepoPathStorage::SqliteSnapshot,
        }],
        conflicted: vec!["assets/model.bin".to_string()],
        conflicted_changes: vec![RepoConflictChange {
            path: "assets/model.bin".to_string(),
            kind: RepoTrackedPathKind::BinaryFile,
            storage: RepoPathStorage::External,
        }],
        branches: Vec::new(),
        remotes: Vec::new(),
        upstream: None,
        upstream_status: None,
        ahead: 0,
        behind: 0,
    };

    let sqlite_status =
        filter_repo_status_by_kind(status.clone(), Some(RepoTrackedPathKind::SqliteDatabase));
    assert_eq!(sqlite_status.unstaged, vec!["app.db"]);
    assert_eq!(sqlite_status.staged, vec!["app.db"]);
    assert!(sqlite_status.conflicted.is_empty());
    assert_eq!(sqlite_status.counts.unstaged, 1);
    assert_eq!(sqlite_status.counts.staged, 1);
    assert_eq!(sqlite_status.counts.conflicted, 0);
    assert!(sqlite_status.has_unstaged_changes);
    assert!(sqlite_status.has_staged_changes);
    assert!(!sqlite_status.has_conflicts);
    assert_eq!(sqlite_status.paths.len(), 1);
    assert_eq!(
        sqlite_status.paths[0].kind,
        RepoTrackedPathKind::SqliteDatabase
    );

    let binary_status = filter_repo_status_by_kind(status, Some(RepoTrackedPathKind::BinaryFile));
    assert!(binary_status.unstaged.is_empty());
    assert!(binary_status.staged.is_empty());
    assert_eq!(binary_status.conflicted, vec!["assets/model.bin"]);
    assert_eq!(binary_status.counts.conflicted, 1);
    assert!(binary_status.has_conflicts);
    assert_eq!(binary_status.paths.len(), 1);
    assert_eq!(binary_status.paths[0].kind, RepoTrackedPathKind::BinaryFile);
    assert_eq!(binary_status.paths[0].storage, RepoPathStorage::External);
}

#[test]
fn ls_files_kind_filter_selects_matching_path_kinds() {
    let paths = vec![
        RepoTrackedPath {
            path: "app.db".to_string(),
            kind: RepoTrackedPathKind::SqliteDatabase,
            storage: RepoPathStorage::SqliteSnapshot,
            size: None,
            page_count: None,
        },
        RepoTrackedPath {
            path: "assets/readme.md".to_string(),
            kind: RepoTrackedPathKind::TextFile,
            storage: RepoPathStorage::Inline,
            size: Some(12),
            page_count: None,
        },
        RepoTrackedPath {
            path: "assets/model.bin".to_string(),
            kind: RepoTrackedPathKind::BinaryFile,
            storage: RepoPathStorage::External,
            size: Some(4096),
            page_count: None,
        },
    ];
    let sqlite_paths =
        filter_tracked_paths_by_kind(paths, Some(RepoTrackedPathKind::SqliteDatabase));
    assert_eq!(sqlite_paths.len(), 1);
    assert_eq!(sqlite_paths[0].path, "app.db");

    let entries = vec![
        RepoTrackedPathEntry {
            path: "app.db".to_string(),
            stage: graft::repo::index::IndexStage::Normal,
            kind: RepoTrackedPathKind::SqliteDatabase,
            storage: RepoPathStorage::SqliteSnapshot,
            mode: None,
            oid: None,
            size: None,
            page_count: None,
        },
        RepoTrackedPathEntry {
            path: "assets/model.bin".to_string(),
            stage: graft::repo::index::IndexStage::Normal,
            kind: RepoTrackedPathKind::BinaryFile,
            storage: RepoPathStorage::External,
            mode: None,
            oid: None,
            size: Some(4096),
            page_count: None,
        },
    ];
    let binary_entries =
        filter_tracked_path_entries_by_kind(entries, Some(RepoTrackedPathKind::BinaryFile));
    assert_eq!(binary_entries.len(), 1);
    assert_eq!(binary_entries[0].path, "assets/model.bin");
}

#[test]
fn parse_repo_config_set_arg_preserves_values_with_spaces() {
    assert_eq!(
        parse_repo_config_set_arg("files.inline_text_threshold -- 8 MB").unwrap(),
        (
            "files.inline_text_threshold".to_string(),
            "8 MB".to_string()
        )
    );
    assert_eq!(
        parse_repo_config_set_arg("files.inline_text_threshold 4 B").unwrap(),
        ("files.inline_text_threshold".to_string(), "4 B".to_string())
    );
    assert!(parse_repo_config_set_arg("files.inline_text_threshold -- ").is_err());
    assert!(parse_repo_config_set_arg("").is_err());
}

#[test]
fn parse_tag_create_arg_supports_annotated_messages() {
    assert_eq!(
        parse_tag_create_arg("v1.0 HEAD").unwrap(),
        ("v1.0".to_string(), Some("HEAD".to_string()), None)
    );
    assert_eq!(
        parse_tag_create_arg("--annotated v1.0 HEAD -- release 1.0").unwrap(),
        (
            "v1.0".to_string(),
            Some("HEAD".to_string()),
            Some("release 1.0".to_string())
        )
    );
    assert_eq!(
        parse_tag_create_arg("-a v1.0 -- release 1.0").unwrap(),
        ("v1.0".to_string(), None, Some("release 1.0".to_string()))
    );
    assert!(parse_tag_create_arg("--annotated v1.0 HEAD").is_err());
    assert!(parse_tag_create_arg("--annotated v1.0 HEAD -- ").is_err());
}

#[test]
fn parse_json_tag_pragmas() {
    let create = Pragma {
        name: "graft_json_tag_create",
        arg: Some("v1.0 HEAD"),
    };
    assert!(matches!(
        GraftPragma::try_from(&create).unwrap(),
        GraftPragma::JsonTagCreate {
            name,
            target: Some(target),
            message: None,
        } if name == "v1.0" && target == "HEAD"
    ));

    let annotated = Pragma {
        name: "graft_json_tag_create",
        arg: Some("--annotated v1.0 HEAD -- release 1.0"),
    };
    assert!(matches!(
        GraftPragma::try_from(&annotated).unwrap(),
        GraftPragma::JsonTagCreate {
            name,
            target: Some(target),
            message: Some(message),
        } if name == "v1.0" && target == "HEAD" && message == "release 1.0"
    ));

    let delete = Pragma {
        name: "graft_json_tag_delete",
        arg: Some("v1.0"),
    };
    assert!(matches!(
        GraftPragma::try_from(&delete).unwrap(),
        GraftPragma::JsonTagDelete { name } if name == "v1.0"
    ));
}

#[test]
fn parse_checkout_and_switch_force_args() {
    assert_eq!(
        parse_repo_checkout_arg("--force HEAD~1").unwrap(),
        RepoCheckoutSpec::Detach { rev: "HEAD~1".to_string(), force: true }
    );
    assert_eq!(
        parse_repo_checkout_arg("HEAD~1 -- app.db").unwrap(),
        RepoCheckoutSpec::Path {
            rev: "HEAD~1".to_string(),
            path: "app.db".to_string(),
        }
    );
    assert!(parse_repo_checkout_arg("--force HEAD~1 -- app.db").is_err());
    assert_eq!(
        parse_repo_restore_arg("external.db").unwrap(),
        RepoRestoreSpec {
            source: None,
            staged: false,
            all: false,
            kind: None,
            path: Some(PathBuf::from("external.db")),
        }
    );
    assert_eq!(
        parse_repo_restore_arg("--source HEAD~1 -- external.db").unwrap(),
        RepoRestoreSpec {
            source: Some("HEAD~1".to_string()),
            staged: false,
            all: false,
            kind: None,
            path: Some(PathBuf::from("external.db")),
        }
    );
    assert_eq!(
        parse_repo_restore_arg("--staged -- external.db").unwrap(),
        RepoRestoreSpec {
            source: None,
            staged: true,
            all: false,
            kind: None,
            path: Some(PathBuf::from("external.db")),
        }
    );
    assert_eq!(
        parse_repo_restore_arg("--staged --source HEAD -- external.db").unwrap(),
        RepoRestoreSpec {
            source: Some("HEAD".to_string()),
            staged: true,
            all: false,
            kind: None,
            path: Some(PathBuf::from("external.db")),
        }
    );
    assert_eq!(
        parse_repo_restore_arg("--staged --all --kind db").unwrap(),
        RepoRestoreSpec {
            source: None,
            staged: true,
            all: true,
            kind: Some(RepoTrackedPathKind::SqliteDatabase),
            path: None,
        }
    );
    assert!(parse_repo_restore_arg("--all").is_err());
    assert!(parse_repo_restore_arg("--kind db -- external.db").is_err());
    assert!(parse_repo_restore_arg("--staged --all external.db").is_err());
    assert!(parse_repo_restore_arg("--staged --all --kind nope").is_err());
    assert_eq!(
        parse_repo_export_arg("--output snapshot.db").unwrap(),
        RepoExportSpec {
            source: None,
            path: None,
            output: PathBuf::from("snapshot.db"),
        }
    );
    assert_eq!(
        parse_repo_export_arg("--source HEAD~1 --output snapshot.db -- app.db").unwrap(),
        RepoExportSpec {
            source: Some("HEAD~1".to_string()),
            path: Some(PathBuf::from("app.db")),
            output: PathBuf::from("snapshot.db"),
        }
    );
    assert_eq!(
        parse_repo_export_arg("--output '/tmp/Application Support/snapshot.db'").unwrap(),
        RepoExportSpec {
            source: None,
            path: None,
            output: PathBuf::from("/tmp/Application Support/snapshot.db"),
        }
    );
    assert_eq!(
        parse_repo_export_arg(
            "--source HEAD --output \"/tmp/Application Support/snapshot.db\" -- \"app data.db\""
        )
        .unwrap(),
        RepoExportSpec {
            source: Some("HEAD".to_string()),
            path: Some(PathBuf::from("app data.db")),
            output: PathBuf::from("/tmp/Application Support/snapshot.db"),
        }
    );
    assert_eq!(
        parse_repo_export_arg("fixtures/users.db -o snapshot.db").unwrap(),
        RepoExportSpec {
            source: None,
            path: Some(PathBuf::from("fixtures/users.db")),
            output: PathBuf::from("snapshot.db"),
        }
    );
    assert!(parse_repo_export_arg("--source HEAD").is_err());

    let json_export = Pragma {
        name: "graft_json_export",
        arg: Some("--source HEAD --output snapshot.db -- app.db"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_export).unwrap(),
        GraftPragma::JsonExport {
            spec: RepoExportSpec {
                source: Some(source),
                path: Some(path),
                output,
            }
        } if source == "HEAD"
            && path == PathBuf::from("app.db")
            && output == PathBuf::from("snapshot.db")
    ));

    assert_eq!(
        parse_switch_branch_arg("--force main").unwrap(),
        ("main".to_string(), true)
    );
    assert_eq!(
        parse_branch_rename_arg("feature/query").unwrap(),
        (None, "feature/query".to_string(), false)
    );
    assert_eq!(
        parse_branch_rename_arg("feature/search feature/query").unwrap(),
        (
            Some("feature/search".to_string()),
            "feature/query".to_string(),
            false
        )
    );
    assert_eq!(
        parse_branch_rename_arg("-M feature/search feature/query").unwrap(),
        (
            Some("feature/search".to_string()),
            "feature/query".to_string(),
            true
        )
    );
    assert_eq!(
        parse_switch_create_arg("-f feature/search HEAD").unwrap(),
        ("feature/search".to_string(), Some("HEAD".to_string()), true)
    );
    let json_switch_branch = Pragma {
        name: "graft_json_switch_branch",
        arg: Some("--force main"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_switch_branch).unwrap(),
        GraftPragma::JsonSwitchBranch { name, force: true } if name == "main"
    ));
    let json_switch_create = Pragma {
        name: "graft_json_switch_create",
        arg: Some("-f feature/search HEAD"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_switch_create).unwrap(),
        GraftPragma::JsonSwitchCreate {
            name,
            start_point: Some(start_point),
            force: true,
        } if name == "feature/search" && start_point == "HEAD"
    ));
    let json_branch_create = Pragma {
        name: "graft_json_branch_create",
        arg: Some("feature/search HEAD"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_branch_create).unwrap(),
        GraftPragma::JsonBranchCreate {
            name,
            start_point: Some(start_point),
        } if name == "feature/search" && start_point == "HEAD"
    ));
    let json_branch_delete = Pragma {
        name: "graft_json_branch_delete",
        arg: Some("--force feature/search"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_branch_delete).unwrap(),
        GraftPragma::JsonBranchDelete { name, force: true } if name == "feature/search"
    ));
    let json_branch_rename = Pragma {
        name: "graft_json_branch_rename",
        arg: Some("feature/search feature/query"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_branch_rename).unwrap(),
        GraftPragma::JsonBranchRename {
            old: Some(old),
            new,
            force: false,
        } if old == "feature/search" && new == "feature/query"
    ));
    let json_branch_upstream = Pragma {
        name: "graft_json_branch_upstream",
        arg: Some("feature/query origin/main"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_branch_upstream).unwrap(),
        GraftPragma::JsonBranchUpstream {
            branch: Some(branch),
            remote,
            remote_branch,
        } if branch == "feature/query" && remote == "origin" && remote_branch == "main"
    ));
    let json_branch_unset_upstream = Pragma {
        name: "graft_json_branch_unset_upstream",
        arg: Some("feature/query"),
    };
    assert!(matches!(
        GraftPragma::try_from(&json_branch_unset_upstream).unwrap(),
        GraftPragma::JsonBranchUnsetUpstream { branch: Some(branch) }
            if branch == "feature/query"
    ));
}
