// Copyright (c) Microsoft Corporation. All rights reserved.

use super::*;
use std::collections::BTreeMap;
use std::io::Read;

const MAX_BATCHES: usize = 512;
const MAX_COLLECTION_PASSES: usize = 64;

fn number(value: &Value, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing numeric observation {pointer}: {value}"))
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap()
}

fn distribution(samples: &[u64]) -> Value {
    assert!(
        !samples.is_empty(),
        "latency must contain real observations"
    );
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    json!({
        "samples":sorted.len(),"minimum_us":sorted[0],
        "p50_us":sorted[(sorted.len()*50).div_ceil(100)-1],
        "p95_us":sorted[(sorted.len()*95).div_ceil(100)-1],
        "maximum_us":sorted[sorted.len()-1],
        "mean_us":sorted.iter().sum::<u64>() as f64 / sorted.len() as f64
    })
}

struct Survey<'a> {
    daemons: Vec<&'a Daemon>,
    queries: Vec<(&'a Daemon, &'a Path)>,
}

struct Observations {
    queries: Vec<Vec<u64>>,
    maxima: Vec<BTreeMap<String, u64>>,
    latest: Vec<Value>,
    batches: usize,
}

impl Observations {
    fn new(survey: &Survey<'_>) -> Self {
        assert!((1..=2).contains(&survey.daemons.len()));
        Self {
            queries: vec![Vec::new(); survey.queries.len()],
            maxima: vec![BTreeMap::new(); survey.daemons.len()],
            latest: vec![Value::Null; survey.daemons.len()],
            batches: 0,
        }
    }

    fn sample(&mut self, survey: &Survey<'_>) {
        assert!(
            self.batches < MAX_BATCHES,
            "bounded sampling budget exhausted"
        );
        for (index, (daemon, root)) in survey.queries.iter().enumerate() {
            let started = Instant::now();
            let result = search(daemon, root);
            self.queries[index].push(micros(started));
            assert_eq!(result["backend"], "shared-v2");
            assert_eq!(result["ready"], true);
        }
        for (index, daemon) in survey.daemons.iter().enumerate() {
            let status = daemon.rpc("namespace.status", json!({}));
            let limits = &status["policy"]["policy"]["work"];
            let allocation = &status["allocation"];
            let usage = &status["usage"];
            assert!(
                number(usage, "/reserved_staging_bytes")
                    <= number(limits, "/staging_bytes").min(number(allocation, "/staging_bytes")),
                "{status}"
            );
            assert!(
                number(usage, "/reserved_private_bytes")
                    + number(usage, "/memory/unreserved_retained_private_estimate_bytes")
                    <= number(limits, "/private_work_bytes")
                        .min(number(allocation, "/private_work_bytes")),
                "{status}"
            );
            assert!(
                number(usage, "/reserved_slots")
                    <= number(limits, "/workers").min(number(allocation, "/work_slots")),
                "{status}"
            );
            assert!(
                number(usage, "/unconsumed_staging_bytes")
                    <= number(usage, "/reserved_staging_bytes"),
                "{status}"
            );
            for pointer in [
                "/usage/storage/known_logical_bytes",
                "/usage/storage/charged_overlap_logical_bytes",
                "/usage/reserved_staging_bytes",
                "/usage/reserved_private_bytes",
                "/usage/reserved_slots",
                "/usage/memory/retained_private_estimate_bytes",
                "/usage/memory/peak_retained_private_estimate_bytes",
                "/usage/memory/mapped_bytes",
                "/usage/memory/peak_mapped_bytes",
            ] {
                let value = number(&status, pointer);
                self.maxima[index]
                    .entry(pointer.into())
                    .and_modify(|maximum| *maximum = (*maximum).max(value))
                    .or_insert(value);
            }
            assert_eq!(status["process_memory"]["scope"], "daemon-process");
            for field in [
                "resident_bytes",
                "resident_peak_bytes",
                "private_bytes",
                "private_high_water_bytes",
            ] {
                let measured = &status["process_memory"][field];
                match measured["status"].as_str() {
                    Some("observed") => {
                        let value = number(measured, "/value");
                        self.maxima[index]
                            .entry(format!("/process_memory/{field}"))
                            .and_modify(|maximum| *maximum = (*maximum).max(value))
                            .or_insert(value);
                    }
                    Some("unavailable") => assert!(measured["reason"].is_string()),
                    _ => panic!("untyped native memory observation: {measured}"),
                }
            }
            self.latest[index] = json!({
                "policy":status["policy"],"allocation":status["allocation"],
                "usage":usage,"process_memory":status["process_memory"],
                "scheduler":status["scheduler"]
            });
        }
        self.batches += 1;
    }

    fn finish(self) -> Value {
        assert!(self.batches > 0);
        json!({
            "sample_batches":self.batches,
            "query_latency":self.queries.iter().map(|samples| distribution(samples)).collect::<Vec<_>>(),
            "query_measurement":"lookup plus search JSON-RPC, fresh connection per request; CLI spawn excluded",
            "sampled_maxima":self.maxima,"last_namespace_observations":self.latest,
            "peak_semantics":"sampled maxima are lower bounds; explicit OS and retained-allocation high-water fields keep their own scopes"
        })
    }
}

fn issue(
    daemon: &Daemon,
    claim: &OwnerClaim,
    sequence: &mut u64,
    method: &str,
    request: Value,
) -> Value {
    let input = json!({"token":token(claim,*sequence),"request":request});
    *sequence += 1;
    daemon.rpc(method, input)
}

fn publication(daemon: &Daemon, result: &Value) -> Value {
    if !result["current"].is_object() {
        return Value::Null;
    }
    let generation = daemon.rpc(
        "objects.inspect",
        json!({"id":result["current"]["current"]["incarnation"]}),
    );
    let checkpoint = daemon.rpc(
        "objects.inspect",
        json!({"id":result["current"]["checkpoint"]}),
    );
    json!({
        "generation_logical_bytes":number(&generation,"/logical_bytes"),
        "new_generation_published_logical_bytes":if result["build"]["published"]==true {
            number(&generation,"/logical_bytes")
        } else {0},
        "checkpoint_published_logical_bytes":number(&checkpoint,"/logical_bytes"),
        "semantics":"sealed output sizes; excludes sort spill and repeated writes, not physical device I/O"
    })
}

fn measure(
    daemon: &Daemon,
    claim: &OwnerClaim,
    sequence: &mut u64,
    method: &str,
    request: Value,
    survey: &Survey<'_>,
    barrier: Option<&str>,
) -> Value {
    let hook = barrier.map(|point| super::faults::pause(daemon, point));
    let started = Instant::now();
    let operation = issue(daemon, claim, sequence, method, request);
    let mut observations = Observations::new(survey);
    let mut preparing_batches = 0;
    let mut barrier_reached = false;
    if let Some(hook) = &hook {
        loop {
            let status = daemon.rpc("testing.status", json!({}));
            if status["stage"] == "waiting" {
                assert_eq!(
                    status["reached_operation"],
                    operation["id"],
                    "{status}; reached: {}",
                    daemon.rpc(
                        "operations.inspect",
                        json!({"id":status["reached_operation"]})
                    )
                );
                observations.sample(survey);
                preparing_batches += 1;
                barrier_reached = true;
                break;
            }
            let current = daemon.rpc("operations.inspect", json!({"id":operation["id"]}));
            if matches!(
                current["state"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "{status}; {current}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        daemon.rpc("testing.release", json!({"id":hook["ticket"]}));
    }
    loop {
        let before = daemon.rpc("operations.inspect", json!({"id":operation["id"]}));
        observations.sample(survey);
        let after = daemon.rpc("operations.inspect", json!({"id":operation["id"]}));
        if before["state"] == "preparing" && after["state"] == "preparing" {
            preparing_batches += 1;
        }
        if matches!(
            after["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            assert_eq!(after["state"], "completed", "{after}");
            return json!({
                "method":method,"end_to_end_observed_us":micros(started),
                "injected_barrier":barrier,"barrier_reached":barrier_reached,
                "preparing_query_batches":preparing_batches,
                "build":after["result"]["build"],"reconcile":after["result"]["reconcile"],
                "publication":publication(daemon,&after["result"]),
                "collection":if method=="collections.start" {after["result"].clone()} else {Value::Null},
                "observations":observations.finish()
            });
        }
        assert!(started.elapsed() < Duration::from_secs(60), "{after}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn attach_request(fixture: &Fixture, claim: &OwnerClaim, root: &Path, lease: &str) -> Value {
    let mut request = attach_input(fixture, claim)["request"].clone();
    request["root"] = json!(fs::canonicalize(root).unwrap());
    request["lease"] = json!(lease);
    request
}

fn ready_attachment(
    daemon: &Daemon,
    claim: &OwnerClaim,
    sequence: &mut u64,
    request: Value,
    survey: &Survey<'_>,
) -> Value {
    let measured = measure(
        daemon,
        claim,
        sequence,
        "views.attach",
        request,
        survey,
        None,
    );
    json!({
        "request_to_ready_us":measured["end_to_end_observed_us"],
        "build":measured["build"],"reconcile":measured["reconcile"],
        "publication":measured["publication"],"observations":measured["observations"]
    })
}

fn update_policy(daemon: &Daemon, claim: &OwnerClaim, sequence: &mut u64, policy: &Value) {
    let status = daemon.rpc("namespace.status", json!({}));
    completed(
        daemon,
        &issue(
            daemon,
            claim,
            sequence,
            "metadata.start",
            json!({
                "method":"policy","params":{"expected_version":status["policy"]["version"],"policy":policy}
            }),
        ),
    );
}

fn extend_fixture(files: usize, bytes: usize, repository: usize) -> Fixture {
    let mut fixture = Fixture::new();
    fs::create_dir(fixture.a.join("corpus")).unwrap();
    for index in 0..files {
        let mut contents = format!("shared_term repository-{repository} file-{index}\n");
        let line = format!(
            "{} generated input\n",
            blake3::hash(format!("{repository}:{index}").as_bytes()).to_hex()
        );
        while contents.len() < bytes {
            contents.push_str(&line);
        }
        contents.truncate(bytes - 1);
        contents.push('\n');
        fs::write(
            fixture.a.join("corpus").join(format!("{index:04}.txt")),
            contents,
        )
        .unwrap();
    }
    git(&fixture.a, &["add", "corpus"]);
    git(
        &fixture.a,
        &["commit", "-qm", "bounded qualification corpus"],
    );
    fixture.revision = git(&fixture.a, &["rev-parse", "HEAD"]);
    git(
        &fixture.b,
        &["checkout", "-q", "--detach", &fixture.revision],
    );
    fixture
}

fn filesystem(root: &Path) -> Value {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = fs::File::open(root).unwrap();
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: the directory is live and the output has the native statfs layout.
        assert_eq!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) }, 0);
        #[cfg(target_os = "linux")]
        return json!({"method":"fstatfs","native_type_hex":format!("{:#x}",info.f_type)});
        #[cfg(target_os = "macos")]
        return json!({"method":"fstatfs","name":unsafe {
            std::ffi::CStr::from_ptr(info.f_fstypename.as_ptr())
        }.to_str().unwrap()});
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return json!({"status":"unavailable","reason":"filesystem-name-not-implemented-on-this-platform"});
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
        let path: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut volume = [0_u16; 32768];
        let mut name = [0_u16; 256];
        // SAFETY: both APIs receive valid NUL-terminated input and sized output buffers.
        assert_ne!(
            unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        assert_ne!(
            unsafe {
                GetVolumeInformationW(
                    volume.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let length = name.iter().position(|unit| *unit == 0).unwrap();
        json!({"method":"GetVolumeInformationW","name":String::from_utf16(&name[..length]).unwrap()})
    }
}

fn provenance() -> Value {
    fn visit(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "source input is a symlink"
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &relative.join(entry.unwrap().file_name()), files);
            }
        } else {
            assert!(metadata.is_file() && files.len() < 4096);
            files.push(relative.to_path_buf());
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let inputs = [
        "Cargo.toml",
        "Cargo.lock",
        "tgrep-cli/Cargo.toml",
        "tgrep-cli/build.rs",
        "tgrep-cli/src",
        "tgrep-cli/tests/shared_daemon.rs",
        "tgrep-cli/tests/shared_daemon",
        "tgrep-core/Cargo.toml",
        "tgrep-core/src",
        "vendor/ignore/Cargo.toml",
        "vendor/ignore/Cargo.lock",
        "vendor/ignore/src",
    ];
    let mut files = Vec::new();
    for input in inputs {
        let relative: PathBuf = input.split('/').collect();
        visit(root, &relative, &mut files);
    }
    files.sort();
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0_u64;
    for relative in &files {
        let contents = fs::read(root.join(relative)).unwrap();
        bytes += contents.len() as u64;
        assert!(bytes <= 128 * 1024 * 1024, "source fingerprint byte bound");
        hash.update(relative.to_str().unwrap().replace('\\', "/").as_bytes());
        hash.update(&[0]);
        hash.update(&(contents.len() as u64).to_le_bytes());
        hash.update(&contents);
    }
    let binary = assert_cmd::cargo::cargo_bin("tgrep");
    let mut executable = fs::File::open(&binary).unwrap();
    assert!(executable.metadata().unwrap().len() <= 256 * 1024 * 1024);
    let mut binary_hash = blake3::Hasher::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let read = executable.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        binary_hash.update(&buffer[..read]);
    }
    let revision = runtime::output(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"]),
    );
    let revision = if revision.status.success() {
        json!({"status":"observed","value":String::from_utf8(revision.stdout).unwrap().trim(),
            "semantics":"base commit only; the input fingerprint includes uncommitted source"})
    } else {
        json!({"status":"unavailable","reason":"source-checkout-revision-unavailable",
            "exit_code":revision.status.code()})
    };
    json!({
        "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "available_parallelism":std::thread::available_parallelism().unwrap().get(),
        "rustc":success(runtime::output(Command::new("rustc").args(["--version","--verbose"]))),
        "git":success(runtime::output(Command::new("git").arg("--version"))),
        "source_revision":revision,"source_inputs":inputs,"source_input_files":files.len(),
        "source_input_bytes":bytes,"source_inputs_blake3":hash.finalize().to_hex().to_string(),
        "binary_blake3":binary_hash.finalize().to_hex().to_string(),
        "binary_profile":binary.parent().unwrap().file_name().unwrap().to_str().unwrap(),
        "managed_test_hooks":true
    })
}

fn qualification(files: usize, bytes: usize, instrumented: bool) -> Value {
    let provenance = provenance();
    let mut first = extend_fixture(files, bytes, 0);
    let second = extend_fixture(files, bytes, 1);
    let filesystem = filesystem(first.temp.path());
    let mut configured = policy();
    configured["collection"]["schedule"] = json!({"mode":"periodic","interval_ms":1000});
    let start_a = Instant::now();
    let mut a = start(&first, &configured, &["--no-watch"]);
    let routing_a = micros(start_a);
    let start_b = Instant::now();
    let mut b = Daemon::start(
        &second.a,
        &first.storage,
        &[
            "--shared-policy",
            first.storage.join("policy.json").to_str().unwrap(),
            "--no-watch",
        ],
    );
    let routing_b = micros(start_b);
    assert_ne!(a.marker["namespace"], b.marker["namespace"]);
    assert_ne!(a.marker["repository"], b.marker["repository"]);
    let (owner_a, guard_a) = owner(&a);
    let (owner_b, guard_b) = owner(&b);
    let (mut sequence_a, mut sequence_b) = (1, 1);
    let mut cold = Vec::new();
    let mut ready_queries = Vec::new();
    for (daemon, fixture, claim, sequence) in [
        (&a, &first, &owner_a, &mut sequence_a),
        (&b, &second, &owner_b, &mut sequence_b),
    ] {
        for (index, root) in [&fixture.a, &fixture.b].into_iter().enumerate() {
            let attachment = ready_attachment(
                daemon,
                claim,
                sequence,
                attach_request(fixture, claim, root, &format!("view-{index}")),
                &Survey {
                    daemons: vec![&a, &b],
                    queries: ready_queries.clone(),
                },
            );
            assert_eq!(attachment["build"]["published"], index == 0, "{attachment}");
            assert_eq!(
                attachment["build"]["reused_generation"],
                index != 0,
                "{attachment}"
            );
            if index != 0 {
                assert_eq!(attachment["build"]["blobs_read"], 0, "{attachment}");
            }
            cold.push(attachment);
            assert_scan_parity(daemon, root, fixture.temp.path());
            ready_queries.push((daemon, root.as_path()));
        }
    }
    let old = a.rpc(
        "lookup",
        json!({"root":fs::canonicalize(&first.a).unwrap()}),
    );
    let old_directory = PathBuf::from(a.marker["storage"].as_str().unwrap())
        .join("objects")
        .join(old["incarnation"].as_str().unwrap());
    let third = first.third(&first.revision);
    for index in 0..files.div_ceil(4) {
        fs::write(
            third.join("corpus").join(format!("{index:04}.txt")),
            format!(
                "shared_term updated-{index}\n{}",
                "new committed input\n".repeat(bytes / 20)
            ),
        )
        .unwrap();
    }
    git(&third, &["commit", "-qam", "bounded successor"]);
    let successor = git(&third, &["rev-parse", "HEAD"]);
    let mut request = attach_request(&first, &owner_a, &third, "view-2");
    request["revision"] = json!(successor);
    let mut phases = Vec::new();
    let survey = Survey {
        daemons: vec![&a, &b],
        queries: vec![
            (&a, &first.a),
            (&a, &first.b),
            (&b, &second.a),
            (&b, &second.b),
        ],
    };
    let build = measure(
        &a,
        &owner_a,
        &mut sequence_a,
        "views.attach",
        request,
        &survey,
        instrumented.then_some("generation-built"),
    );
    assert_eq!(build["build"]["published"], true, "{build}");
    assert!(number(&build, "/build/blobs_read") > 0);
    phases.push(build);
    for root in [&first.b, &first.a] {
        git(root, &["checkout", "-q", "--detach", &successor]);
        let view = a.rpc("lookup", json!({"root":fs::canonicalize(root).unwrap()}));
        let survey = Survey {
            daemons: vec![&a, &b],
            queries: vec![
                (&a, if root == &first.b { &first.a } else { &first.b }),
                (&a, &third),
                (&b, &second.a),
                (&b, &second.b),
            ],
        };
        let migration = measure(
            &a,
            &owner_a,
            &mut sequence_a,
            "views.advance",
            json!({
                "root":view["root"],"view":view["view"],"expected_version":view["version"],
                "target_commit":successor,"profile":serde_json::from_str::<Value>(PROFILE).unwrap(),
                "owner":owner_a.owner,"allocation_version":1
            }),
            &survey,
            instrumented.then_some("migration-prepared"),
        );
        assert_eq!(migration["build"]["reused_generation"], true, "{migration}");
        assert_eq!(migration["build"]["blobs_read"], 0, "{migration}");
        assert_scan_parity(&a, root, first.temp.path());
        phases.push(migration);
    }
    let mut bounded = configured.clone();
    bounded["retention"] = json!({"mode":"bounded","target_bytes":1});
    // Keep the sibling's scheduled maintenance on; isolate measured collection receipts.
    bounded["collection"]["schedule"] = json!({"mode":"disabled"});
    update_policy(&a, &owner_a, &mut sequence_a, &bounded);
    let drain_started = Instant::now();
    loop {
        let diagnostics = a.rpc("maintenance.status", json!({}));
        if diagnostics["scheduler"]["automatic_collection_enabled"] == false
            && diagnostics["scheduler"]["collection_in_flight"] == false
            && number(&diagnostics, "/aggregate/collection/in_flight") == 0
        {
            break;
        }
        assert!(
            drain_started.elapsed() < Duration::from_secs(30),
            "{diagnostics}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let survey = Survey {
        daemons: vec![&a, &b],
        queries: vec![
            (&a, &first.a),
            (&a, &first.b),
            (&b, &second.a),
            (&b, &second.b),
        ],
    };
    let bounds = json!({"max_duration_ms":1000,"max_examined":16,"max_removed":2,
        "max_delete_bytes":65536,"max_pages":1});
    let mut cursor = Value::Null;
    let mut reclaimed = 0_u64;
    let mut collection_passes = 0;
    let mut collection_synchronized = false;
    for _ in 0..MAX_COLLECTION_PASSES {
        let policy_version = a.rpc("namespace.status", json!({}))["policy"]["version"].clone();
        let measured = measure(
            &a,
            &owner_a,
            &mut sequence_a,
            "collections.start",
            json!({
                "policy_version":policy_version,"allocation_version":1,"bounds":bounds,"cursor":cursor
            }),
            &survey,
            (instrumented && !collection_synchronized).then_some("object-withdrawn"),
        );
        let progress = &measured["collection"];
        for (actual, maximum) in [
            ("examined", "max_examined"),
            ("removed", "max_removed"),
            ("pages", "max_pages"),
            ("logical_bytes_reclaimed", "max_delete_bytes"),
        ] {
            assert!(
                number(progress, &format!("/{actual}")) <= number(&bounds, &format!("/{maximum}")),
                "{progress}"
            );
        }
        assert_eq!(progress["errors"], 0, "{progress}");
        assert!(progress["elapsed_nanos"].is_u64());
        assert!(progress["elapsed_budget_exceeded"].is_boolean());
        reclaimed += number(progress, "/logical_bytes_reclaimed");
        collection_synchronized |= measured["barrier_reached"] == true;
        cursor = progress["next"].clone();
        phases.push(measured);
        collection_passes += 1;
        if !old_directory.exists() {
            break;
        }
    }
    assert!(
        !old_directory.exists(),
        "original generation {} was not collected; last pass: {}; preview: {}",
        old["incarnation"],
        phases.last().unwrap()["collection"],
        a.rpc("collections.preview", json!({}))
    );
    assert!(!instrumented || collection_synchronized);
    assert!(reclaimed > 0);
    assert_scan_parity(&a, &first.a, first.temp.path());
    assert_scan_parity(&b, &second.a, second.temp.path());
    update_policy(&a, &owner_a, &mut sequence_a, &configured);
    let maintenance =
        [&a, &b].map(|daemon| daemon.rpc("maintenance.status", json!({}))["aggregate"].clone());
    for (daemon, claim, count) in [(&a, &owner_a, 3), (&b, &owner_b, 2)] {
        for index in 0..count {
            daemon.rpc(
                "views.detach",
                json!({"owner":claim.owner,"lease":format!("view-{index}")}),
            );
        }
        daemon.rpc("owners.release", json!({"claim":claim}));
    }
    drop((guard_a, guard_b));
    stop(&mut a);
    stop(&mut b);
    first.revision = successor;
    let warm_start = Instant::now();
    let mut warm = start(&first, &configured, &["--no-watch"]);
    let warm_routing = micros(warm_start);
    let (claim, guard) = owner(&warm);
    let mut sequence = 1;
    let attachment = ready_attachment(
        &warm,
        &claim,
        &mut sequence,
        attach_request(&first, &claim, &first.a, "warm-view"),
        &Survey {
            daemons: vec![&warm],
            queries: Vec::new(),
        },
    );
    assert_eq!(attachment["build"]["published"], false, "{attachment}");
    assert_eq!(
        attachment["build"]["reused_generation"], true,
        "{attachment}"
    );
    assert_eq!(attachment["build"]["blobs_read"], 0, "{attachment}");
    assert_scan_parity(&warm, &first.a, first.temp.path());
    let warm_memory = warm.rpc("namespace.status", json!({}))["process_memory"].clone();
    warm.rpc(
        "views.detach",
        json!({"owner":claim.owner,"lease":"warm-view"}),
    );
    warm.rpc("owners.release", json!({"claim":claim}));
    drop(guard);
    stop(&mut warm);
    json!({
        "schema":1,"provenance":provenance,"filesystem":filesystem,
        "fixture":{"repositories":2,"initial_ready_worktrees":4,"additional_worktrees":1,
            "generated_files_per_repository":files,"generated_bytes_per_file":bytes,
            "base_fixture_files_per_repository":3,"modified_fraction":"first ceil(files/4)",
            "cache_condition":"fresh managed stores; OS filesystem caches were not evicted",
            "instrumented_functional_run":instrumented},
        "limits":{"policy":configured,"collection_bounds":bounds,"collection_target_bytes":1,
            "sampling_batches_per_phase":MAX_BATCHES,"maximum_collection_passes":MAX_COLLECTION_PASSES,
            "sampling_interval_ms":5},
        "cold":{"routing_ready_us":[routing_a,routing_b],"attachments":cold},
        "phases":phases,"collection_passes":collection_passes,
        "actual_logical_bytes_reclaimed":reclaimed,"maintenance_aggregates":maintenance,
        "warm":{"routing_ready_us":warm_routing,"attachment":attachment,"process_memory":warm_memory},
        "physical_device_io_bytes":{"status":"unavailable","reason":"kernel-io-not-sampled"},
        "git_child_peak_memory":{"status":"unavailable","reason":"git-child-memory-not-sampled"},
        "interpretation":"bounded representative workload; no universal latency threshold; process memory is not namespace-attributed; observation costs are included"
    })
}

#[test]
fn bounded_lifecycle_queries_and_measurement_shape() {
    let report = qualification(12, 1024, true);
    assert_eq!(report["schema"], 1);
    assert!(report["phases"].as_array().unwrap().len() >= 4);
    assert!(
        report["provenance"]["binary_blake3"]
            .as_str()
            .unwrap()
            .len()
            == 64
    );
    for phase in report["phases"].as_array().unwrap() {
        assert!(number(phase, "/observations/sample_batches") > 0);
        if phase["barrier_reached"] == true {
            assert!(number(phase, "/preparing_query_batches") > 0);
        }
    }
}

#[test]
#[ignore = "deliberate native measurement; requires an exclusive report path and a quiet host"]
fn native_managed_lifecycle_measurement() {
    let path = PathBuf::from(
        std::env::var_os("TGREP_MANAGED_PERFORMANCE_REPORT")
            .expect("set TGREP_MANAGED_PERFORMANCE_REPORT to a new JSON file"),
    );
    assert!(
        path.is_absolute() && !path.exists(),
        "use a new absolute report path"
    );
    let report = qualification(256, 4096, false);
    let bytes = serde_json::to_vec_pretty(&report).unwrap();
    assert!(bytes.len() <= 4 * 1024 * 1024, "bounded report size");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(&bytes).unwrap();
    file.sync_all().unwrap();
    println!("Managed native measurement: {}", path.display());
}
