// Capture / deploy (boot menu [C]) end to end: `bootrom-mgmt capture` on the client side reads a disk and sends it to
// the real server through the upload API, then writes the image back onto a bigger disk — signed in with the owner
// account typed on its input.
mod common;

use common::clients::Http;
use common::*;
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// The client tool with `disk` as the only disk and `input` typed on its console. `tls` = BROOM_TLS (the server's
/// certificate fingerprint as confirmed by the admin; None = the tool asks on its console).
fn tool_with(disk: &std::path::Path, input: &str, tls: Option<&str>) -> (Output, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_bootrom-mgmt"));
    c.args(["capture", SRV]).env("BROOM_CAPTURE_DISK", disk);
    match tls {
        Some(t) => c.env("BROOM_TLS", t),
        None => c.env_remove("BROOM_TLS"),
    };
    let mut c = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    (out, text)
}

fn tool(disk: &std::path::Path, input: &str) -> (Output, String) {
    tool_with(disk, input, FINGERPRINT.get().map(String::as_str))
}

/// The server's certificate fingerprint, as its Settings -> HTTPS page shows it (read once the admin is signed in).
static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn sh(script: &str) {
    assert!(
        Command::new("sh")
            .args(["-ec", script])
            .status()
            .unwrap()
            .success(),
        "{script}"
    );
}

#[test]
fn capture_then_deploy() {
    sandbox("capture_then_deploy", || {
        let home = Home::new("capture");
        // With HTTPS on (the default): the tool talks HTTPS only, to the certificate whose fingerprint the admin
        // confirms — the one on the server's Settings -> HTTPS page.
        let _srv = Server::start_with(&home, &["--https-port", "443"]);
        let cert = std::fs::read_to_string(home.0.join("tls/cert.pem")).unwrap();
        let h = admin_via(&home, &Http::tls(&cert));
        let sha = h.get("/api/config/tls").json()["sha256"]
            .as_str()
            .unwrap()
            .to_string();
        FINGERPRINT.set(sha.clone()).unwrap();

        // 64 MiB GPT disk: p1 8 MiB of data, p2 the rest, zeros but one MiB near its end.
        let disk = home.0.join("pc-disk.img");
        let d = disk.display();
        sh(&format!(
            "truncate -s 64M {d}
             printf 'label: gpt\\nsize=8MiB, type=U\\ntype=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\\n' | sfdisk -q {d}
             yes broom | head -c 8388608 | dd of={d} bs=1M seek=1 conv=notrunc status=none
             yes data | head -c 1048576 | dd of={d} bs=1M seek=50 conv=notrunc status=none"
        ));

        // The certificate is the tool's only trust in the server, and it must be confirmed:
        // another fingerprint given (a host in the middle with its own certificate) → the tool stops before signing in;
        let wrong = format!("{}00", &sha[..sha.len() - 2]).replace("0000", "1100");
        let wrong = if wrong == sha { format!("{}ff", &sha[..sha.len() - 2]) } else { wrong };
        let (out, text) = tool_with(&disk, &format!("admin\n{PASSWORD}\n1\n1\nx\n"), Some(&wrong));
        assert!(!out.status.success(), "{text}");
        assert!(text.contains("not the one given in BROOM_TLS"), "{text}");
        assert!(!text.contains("Owner account"), "stopped before the sign-in: {text}");
        // plain HTTP asked for while the server answers HTTPS → stops as well (no downgrade);
        let (out, text) = tool_with(&disk, "yes\nadmin\n", Some("plain"));
        assert!(!out.status.success() && text.contains("answers HTTPS"), "{text}");
        // asked on its console: shown as on Settings -> HTTPS; anything but yes → stops.
        let (out, text) = tool_with(&disk, "no\n", None);
        assert!(!out.status.success(), "{text}");
        let shown: String = sha
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(":");
        assert!(text.contains(&shown), "its fingerprint {shown} shown: {text}");
        assert!(text.contains("certificate not confirmed"), "{text}");

        // A wrong password first (asked again), the owner's, Capture, disk 1, the image name.
        let (out, text) = tool(
            &disk,
            &format!("admin\nwrong-password\nadmin\n{PASSWORD}\n1\n1\ncap1\n"),
        );
        assert!(text.contains("wrong user or password"), "{text}");
        assert!(text.contains("Image cap1 created"), "{text}");
        // Over HTTPS (plain HTTP admin calls are refused while HTTPS is on), never asking to go plain.
        assert!(!text.contains("plain HTTP"), "{text}");
        assert!(text.contains("All sent"), "{text}");
        // No Windows on this disk: the server's publish refuses it — after the convert.
        assert!(text.contains("could not publish cap1"), "{text}");
        assert_eq!(out.status.code(), Some(1));
        assert!(wait_job(&h, "cap1").starts_with('✗'));
        let img = home.0.join("images/cap1/image.img");
        let src = std::fs::read(&disk).unwrap();
        assert_eq!(
            std::fs::read(&img).unwrap(),
            src,
            "the server's golden is the disk"
        );
        use std::os::unix::fs::MetadataExt;
        assert!(
            std::fs::metadata(&img).unwrap().blocks() * 512 < 20 << 20,
            "zeros stay holes"
        );

        // Owner only, and never a mix of two goldens.
        assert_eq!(
            Http::tls(&cert)
                .get("/api/images/deploy-map?name=cap1")
                .status,
            401
        );
        assert_eq!(
            h.get("/api/images/deploy-chunk?name=cap1&stamp=old&offset=0&len=512")
                .status,
            409
        );

        // Deploy it onto a bigger disk full of old data: typed confirmation, then the disk = the image + its backup
        // GPT moved to the new end.
        let target = home.0.join("new-disk.img");
        let t = target.display();
        sh(&format!("yes old | head -c 104857600 > {t}"));
        // Deploy, image 1, disk 1, the disk's name typed wrong / right.
        let (_, text) = tool(&target, &format!("admin\n{PASSWORD}\n2\n1\n1\nnope\n"));
        assert!(text.contains("stopped by the user"), "{text}");
        let (out, text) = tool(
            &target,
            &format!("admin\n{PASSWORD}\n2\n1\n1\nnew-disk.img\n"),
        );
        assert_eq!(out.status.code(), Some(0), "{text}");
        assert!(text.contains("partition table moved"), "{text}");
        let got = std::fs::read(&target).unwrap();
        assert_eq!(got.len(), 100 << 20);
        // Past the primary table up to the old backup: byte for byte (MBR + header point to the new end now).
        let diff = (34 * 512..63 << 20).find(|&i| got[i] != src[i]);
        assert_eq!(diff, None, "first byte that differs");
        let v = Command::new("sfdisk")
            .args(["--verify", &t.to_string()])
            .output()
            .unwrap();
        assert!(v.status.success(), "{}", String::from_utf8_lossy(&v.stdout));
        let list = |p: &str| {
            String::from_utf8_lossy(
                &Command::new("sfdisk")
                    .args(["-d", p])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .lines()
            .filter(|l| l.contains(": start="))
            .map(|l| l.split_once(':').unwrap().1.to_string())
            .collect::<Vec<_>>()
        };
        assert_eq!(
            list(&t.to_string()),
            list(&d.to_string()),
            "same partitions"
        );
    });
}
