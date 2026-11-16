//! End-to-end test: two real pingchess processes play Fool's Mate over
//! loopback ICMP.
//!
//! This needs CAP_NET_RAW, so it is ignored by default. Run it with:
//!
//! ```text
//! scripts/test-icmp.sh
//! ```
//!
//! The script creates a private user+network namespace, brings loopback up,
//! and runs this test there.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Outputs {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_pair(
    host_args: &[&str],
    join_args: &[&str],
    host_input: &[u8],
    join_input: &[u8],
) -> (Outputs, Outputs) {
    let binary = env!("CARGO_BIN_EXE_pingchess");
    let common = [
        "--ascii",
        "--no-color",
        "--timeout",
        "750",
        "--retries",
        "40",
    ];

    let mut host = Command::new(binary)
        .arg("host")
        .args(common)
        .args(host_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn host");

    thread::sleep(Duration::from_millis(150));

    let mut join = Command::new(binary)
        .arg("join")
        .arg("127.0.0.1")
        .args(common)
        .args(join_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn joiner");

    host.stdin
        .take()
        .expect("host stdin")
        .write_all(host_input)
        .expect("write host moves");
    join.stdin
        .take()
        .expect("join stdin")
        .write_all(join_input)
        .expect("write join moves");

    let host_output = collect(&mut host);
    let join_output = collect(&mut join);
    (host_output, join_output)
}

fn collect(child: &mut Child) -> Outputs {
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break child.wait().expect("wait after kill");
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");

    Outputs {
        success: status.success(),
        stdout,
        stderr,
    }
}

fn assert_game(output: &Outputs, label: &str, our_color: &str) {
    assert!(
        output.success,
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.stderr.is_empty(),
        "{label} wrote to stderr: {}",
        output.stderr
    );
    assert!(
        output.stdout.contains(&format!("You are {our_color}.")),
        "{label} was not assigned {our_color}\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("Checkmate!"),
        "{label} did not report checkmate\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("Qh4#"),
        "{label} did not show the final move\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("Result: 0-1"),
        "{label} did not report 0-1\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("ack for move in"),
        "{label} never received a move acknowledgement\n{}",
        output.stdout
    );
}

fn script_stdin(mut stdin: ChildStdin, steps: Vec<(u64, String)>) {
    for (delay_ms, text) in steps {
        thread::sleep(Duration::from_millis(delay_ms));
        if stdin.write_all(text.as_bytes()).is_err() {
            return;
        }
        let _ = stdin.flush();
    }
}

fn room_client(args: &[&str], steps: &[(u64, &str)]) -> (Child, thread::JoinHandle<()>) {
    let binary = env!("CARGO_BIN_EXE_pingchess");
    let mut child = Command::new(binary)
        .arg("connect")
        .arg("127.0.0.1")
        .args([
            "--ascii",
            "--no-color",
            "--timeout",
            "800",
            "--retries",
            "30",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn room client");
    let stdin = child.stdin.take().expect("room client stdin");
    let steps: Vec<(u64, String)> = steps
        .iter()
        .map(|(delay, text)| (*delay, (*text).to_string()))
        .collect();
    let handle = thread::spawn(move || script_stdin(stdin, steps));
    (child, handle)
}

fn assert_room_client(output: &Outputs, label: &str, color: &str) {
    assert!(
        output.success,
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        output.stdout, output.stderr
    );
    assert!(
        output.stderr.is_empty(),
        "{label} wrote to stderr: {}",
        output.stderr
    );
    assert!(
        output.stdout.contains(&format!("you are {color}")),
        "{label} did not know it was {color}\n{}",
        output.stdout
    );
    assert!(
        output
            .stdout
            .contains("New game: alice (White) vs bob (Black)."),
        "{label} did not see the game start\n{}",
        output.stdout
    );
    for needle in ["plays f3", "plays e5", "plays g4", "plays Qh4#"] {
        assert!(
            output.stdout.contains(needle),
            "{label} missed {needle}\n{}",
            output.stdout
        );
    }
    assert!(
        output.stdout.contains("Checkmate! bob wins."),
        "{label} missed the checkmate\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("result 0-1"),
        "{label} missed the result\n{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("bye"),
        "{label} did not exit cleanly\n{}",
        output.stdout
    );
}

#[test]
#[ignore = "requires CAP_NET_RAW; run scripts/test-icmp.sh"]
fn rooms_chat_and_checkmate_over_icmp() {
    let binary = env!("CARGO_BIN_EXE_pingchess");
    let mut hub = Command::new(binary)
        .arg("serve")
        .args(["--name", "test hub"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rooms hub");

    thread::sleep(Duration::from_millis(250));

    let (mut bob, bob_script) = room_client(
        &["--nick", "bob"],
        &[
            (200, "e7e5\nd8h4\n"),
            (6500, "good game\n"),
            (1500, "/quit\n"),
        ],
    );
    let (mut alice, alice_script) = room_client(
        &["--nick", "alice"],
        &[
            (2500, "/new bob\nf2f3\ng2g4\n"),
            (3800, "nice game\n"),
            (1500, "/who\n/quit\n"),
        ],
    );

    let alice_output = collect(&mut alice);
    let bob_output = collect(&mut bob);
    let _ = alice_script.join();
    let _ = bob_script.join();

    let _ = hub.kill();
    let hub_output = collect(&mut hub);

    assert_room_client(&alice_output, "alice", "White");
    assert_room_client(&bob_output, "bob", "Black");

    assert!(
        alice_output.stdout.contains("nice game"),
        "alice did not see her chat\n{}",
        alice_output.stdout
    );
    assert!(
        bob_output.stdout.contains("nice game"),
        "bob did not receive alice's chat\n{}",
        bob_output.stdout
    );
    assert!(
        alice_output.stdout.contains("good game"),
        "alice did not receive bob's chat\n{}",
        alice_output.stdout
    );

    assert!(
        hub_output.stdout.contains("alice vs bob"),
        "hub did not start the game\n{}",
        hub_output.stdout
    );
}

#[test]
#[ignore = "requires CAP_NET_RAW; run scripts/test-icmp.sh"]
fn fools_mate_in_both_color_assignments() {
    let (host, join) = run_pair(
        &["--name", "alice"],
        &["--name", "bob"],
        b"f2f3\ng2g4\n",
        b"e7e5\nd8h4\n",
    );
    assert_game(&host, "host (white)", "White");
    assert_game(&join, "joiner (black)", "Black");

    let (host, join) = run_pair(
        &["--name", "alice", "--as", "black"],
        &["--name", "bob"],
        b"e7e5\nd8h4\n",
        b"f2f3\ng2g4\n",
    );
    assert_game(&host, "host (black)", "Black");
    assert_game(&join, "joiner (white)", "White");
}
