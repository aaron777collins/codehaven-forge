//! Actions that are not built in (ch-ctk.4): JavaScript actions the server fetched, composite
//! actions in the workspace, and the ones the runner refuses. `node` is a stand-in that runs the
//! script with `sh`, so the engine's part (inputs, outputs, state, post steps) is what is tested.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use haven_runner::testing::TestJob;

/// A `node` that runs its script with `sh`, and the PATH that finds it first.
fn fake_node() -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let node = d.path().join("node");
    std::fs::write(&node, "#!/bin/sh\nexec /bin/sh \"$@\"\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", d.path().display());
    (d, path)
}

const GREET: &str = "name: greet
inputs:
  who:
    default: ${{ github.repository }}
  loud:
    default: 'no'
outputs:
  greeting:
    description: what it said
runs:
  using: node20
  main: main.js
  post: post.js
";

#[tokio::test]
async fn a_javascript_action_gets_inputs_saves_state_and_runs_its_post_step_last() {
    let h = Harness::new().await;
    let (_bin, path) = fake_node();
    h.server.add_action(
        "acme/greet@v1",
        &[
            ("action.yml", GREET, 0o644),
            (
                "main.js",
                "echo \"greeting=hello $INPUT_WHO loud=$INPUT_LOUD\" >> \"$GITHUB_OUTPUT\"\necho \"saved=kept\" >> \"$GITHUB_STATE\"\necho \"from $GITHUB_ACTION_REPOSITORY at $GITHUB_ACTION_REF\"\n",
                0o644,
            ),
            ("post.js", "echo \"post saw $STATE_saved\"\n", 0o644),
        ],
    );
    let job = TestJob::new(1)
        .env("PATH", &path)
        .uses_id("g", "acme/greet@v1", &[("who", "world")])
        .run("echo got ${{ steps.g.outputs.greeting }}")
        .build();
    let r = h.run(job).await;
    let log = h.server.log_text(1);
    assert_eq!(r.conclusion, "success", "{log}");
    assert!(h.server.step_log(1, 1).contains("from acme/greet at v1"), "{log}");
    assert!(h.server.step_log(1, 2).contains("got hello world loud=no"), "a default applies when the step gives none: {log}");
    // The post step is a step of its own, after the workflow's, and sees the saved state.
    let updates = h.server.step_updates(1);
    let post = updates.get(&3).expect("a post step was reported");
    assert_eq!(post[0].name.as_deref(), Some("Post acme/greet"));
    assert_eq!(post.last().and_then(|u| u.conclusion.as_deref()), Some("success"));
    assert!(h.server.step_log(1, 3).contains("post saw kept"), "{log}");
}

#[tokio::test]
async fn a_post_step_runs_after_a_failure_and_a_refused_action_names_the_reason() {
    let h = Harness::new().await;
    let (_bin, path) = fake_node();
    h.server.add_action(
        "acme/greet@v1",
        &[("action.yml", GREET, 0o644), ("main.js", "true\n", 0o644), ("post.js", "echo cleaned up\n", 0o644)],
    );
    let job = TestJob::new(2).env("PATH", &path).uses("acme/greet@v1", &[]).run("exit 3").uses("nobody/thing@v1", &[]).build();
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(2, 4).contains("cleaned up"), "post-if defaults to always(): {}", h.server.log_text(2));
    let h2 = Harness::new().await;
    let r = h2.run(TestJob::new(3).uses("nobody/thing@v1", &[]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h2.server.step_log(3, 1).contains("not allowed on this instance"), "{}", h2.server.log_text(3));
}

#[tokio::test]
async fn a_composite_action_in_the_workspace_runs_its_steps_with_its_own_inputs_and_outputs() {
    let h = Harness::new().await;
    // `@@` becomes `$` after writing, so the outer step does not fill in the inner expressions.
    let make = r#"mkdir -p .github/actions/hi && cat > .github/actions/hi/action.yml <<'EOF'
name: hi
inputs:
  name:
    default: you
outputs:
  msg:
    value: @@{{ steps.say.outputs.msg }}
runs:
  using: composite
  steps:
    - id: say
      shell: bash
      run: echo "msg=hi @@{{ inputs.name }}" >> "$GITHUB_OUTPUT"
    - shell: bash
      run: test -f "$GITHUB_ACTION_PATH/action.yml" && echo inner path ok
    - if: @@{{ inputs.name == 'nobody' }}
      shell: bash
      run: echo should not run
EOF
sed -i 's/@@/$/g' .github/actions/hi/action.yml"#;
    let job = TestJob::new(4)
        .run(make)
        .uses_id("c", "./.github/actions/hi", &[("name", "Ann")])
        .run("echo out=${{ steps.c.outputs.msg }} say=${{ steps.say.outputs.msg }}")
        .build();
    let r = h.run(job).await;
    let log = h.server.log_text(4);
    assert_eq!(r.conclusion, "success", "{log}");
    assert!(h.server.step_log(4, 2).contains("inner path ok"), "{log}");
    assert!(!h.server.step_log(4, 2).contains("Run echo should not run"), "{log}");
    // The action's outputs reach the job; its inner step ids do not.
    assert!(h.server.step_log(4, 3).contains("out=hi Ann say="), "{log}");
    assert!(!h.server.step_log(4, 3).contains("say=hi"), "{log}");
}

/// ch-ctk.7: a composite action reads what it is handed, not the job's secrets, variables, needs
/// or matrix, and an input default cannot reach them either. Passing a secret in `with:` works.
#[tokio::test]
async fn a_composite_action_cannot_read_the_jobs_secrets_but_can_be_handed_one() {
    let h = Harness::new().await;
    // `@@` becomes `$` after writing, so the outer step does not fill in the inner expressions.
    let make = r#"mkdir -p .github/actions/peek && cat > .github/actions/peek/action.yml <<'EOF'
name: peek
inputs:
  handed:
    default: ''
  sneaky:
    default: @@{{ secrets.TOPSECRET }}@@{{ vars.SHOWN }}
outputs:
  seen:
    value: @@{{ steps.s.outputs.seen }}
runs:
  using: composite
  steps:
    - id: s
      shell: bash
      run: echo "seen=[@@{{ secrets.TOPSECRET }}][@@{{ vars.SHOWN }}][@@{{ matrix.os }}][@@{{ needs.a.result }}][@@{{ inputs.handed }}][@@{{ inputs.sneaky }}][@@{{ github.repository }}]" >> "$GITHUB_OUTPUT"
EOF
sed -i 's/@@/$/g' .github/actions/peek/action.yml"#;
    let job = TestJob::new(41)
        .secret("TOPSECRET", "s3cr3t-value")
        .var("SHOWN", "visible")
        .run(make)
        .uses_id("p", "./.github/actions/peek", &[("handed", "${{ secrets.TOPSECRET }}")])
        .run("echo out=${{ steps.p.outputs.seen }}")
        .build();
    let r = h.run(job).await;
    let log = h.server.log_text(41);
    assert_eq!(r.conclusion, "success", "{log}");
    let out = h.server.step_log(41, 3);
    // Handed in with:, the value arrives (and the log masks it); read directly, it is empty.
    assert!(out.contains("out=[][][][][***][][alice/demo]"), "{out}\n{log}");
    assert!(!log.contains("s3cr3t-value") && !log.contains("visible"), "{log}");
}

/// ch-ctk.7: composite actions nest to a fixed depth and no further.
#[tokio::test]
async fn composite_actions_nest_to_a_fixed_depth_and_the_next_one_fails_plainly() {
    let h = Harness::new().await;
    let make = r#"mkdir -p .github/actions/deep && cat > .github/actions/deep/action.yml <<'EOF'
name: deep
runs:
  using: composite
  steps:
    - shell: bash
      run: echo level
    - uses: ./.github/actions/deep
EOF"#;
    let r = h.run(TestJob::new(42).run(make).uses("./.github/actions/deep", &[]).build()).await;
    let log = h.server.log_text(42);
    assert_eq!(r.conclusion, "failure", "{log}");
    assert!(log.contains("nested at most 9 deep"), "{log}");
    assert!(log.matches("level").count() >= 9, "it ran to the limit before failing: {log}");
}

#[tokio::test]
async fn a_docker_container_action_is_refused_with_what_to_do_instead() {
    let h = Harness::new().await;
    h.server.add_action("acme/box@v1", &[("action.yml", "name: box\nruns:\n  using: docker\n  image: Dockerfile\n", 0o644)]);
    let r = h.run(TestJob::new(5).uses("acme/box@v1", &[]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(5, 1).contains("docker://image"), "{}", h.server.log_text(5));
}

// ---- services (ch-ctk.7) ----------------------------------------------------------------------

fn service(id: &str, image: &str, port: u16) -> ch_api_types::runner::ServiceSpec {
    ch_api_types::runner::ServiceSpec {
        id: id.into(),
        image: image.into(),
        env: [("POSTGRES_PASSWORD".to_string(), "${{ secrets.DB_PW }}".to_string())].into_iter().collect(),
        ports: vec![port],
        options: Some("--health-cmd pg_isready --health-interval 10s -e EXTRA=1".into()),
    }
}

#[tokio::test]
async fn a_service_shares_the_jobs_network_and_the_job_waits_for_its_port() {
    let h = Harness::new().await;
    // Something listening stands in for the service's port.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let mut job = TestJob::new(6)
        .secret("DB_PW", "pw-123")
        .run("echo ports=${{ join(job.services.db.ports.*, ',') }} id=${{ job.services.db.id != '' }}")
        .build();
    job.services.push(service("db", "postgres:17-alpine", port));
    let r = h.run(job).await;
    let log = h.server.log_text(6);
    assert_eq!(r.conclusion, "success", "{log}");
    assert!(log.contains(&format!("Service db is ready on port {port}")), "{log}");
    assert!(h.server.step_log(6, 1).contains(&format!("ports={port} id=true")), "{log}");
    let created = h.backend.created();
    let job_spec = &created[0];
    let svc = created.iter().find(|c| c.network_of.is_some()).expect("a service container");
    assert!(job_spec.extra_hosts.contains(&"db:127.0.0.1".to_string()), "{:?}", job_spec.extra_hosts);
    assert_eq!(svc.image, "postgres:17-alpine");
    assert_eq!(svc.env.get("POSTGRES_PASSWORD").map(String::as_str), Some("pw-123"), "secrets fill in the service's env");
    assert_eq!(svc.env.get("EXTRA").map(String::as_str), Some("1"), "-e in options adds to the env");
    // The secret never reaches the log, and the service is gone afterwards.
    assert!(!log.contains("pw-123"), "{log}");
    assert_eq!(h.backend.live_containers(), 0);
}

#[tokio::test]
async fn a_service_that_never_listens_fails_the_job_before_any_step() {
    let h = Harness::with_config(|c| c.limits.service_wait_seconds = 1).await;
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let mut job = TestJob::new(7).run("echo should not run").build();
    job.services.push(service("db", "postgres:17-alpine", port));
    let r = h.run(job).await;
    assert_eq!(r.conclusion, "failure");
    let log = h.server.log_text(7);
    assert!(log.contains(&format!("did not take connections on port {port}")), "{log}");
    assert!(!h.server.step_log(7, 1).contains("should not run"), "{log}");
    assert_eq!(h.backend.live_containers(), 0);
}

#[tokio::test]
async fn a_javascript_action_in_an_image_without_node_says_so() {
    let h = Harness::new().await;
    h.server.add_action("acme/greet@v1", &[("action.yml", GREET, 0o644), ("main.js", "true\n", 0o644), ("post.js", "true\n", 0o644)]);
    let r = h.run(TestJob::new(8).env("PATH", "/nonexistent").uses("acme/greet@v1", &[]).build()).await;
    assert_eq!(r.conclusion, "failure");
    assert!(h.server.step_log(8, 1).contains("has no `node`"), "{}", h.server.log_text(8));
}
