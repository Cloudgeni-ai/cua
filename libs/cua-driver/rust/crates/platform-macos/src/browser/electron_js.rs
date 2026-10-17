//! ElectronJS: detect Electron apps and evaluate JS via CDP inspector.

use std::collections::HashSet;
use std::time::Duration;

use super::cdp_client::{listening_ports, CdpClient};

pub struct ElectronJs;

impl ElectronJs {
    /// Returns true if the process at `pid` is an Electron app: an `.app`
    /// bundle containing its executable ships the Electron framework.
    ///
    /// Action tools ask this before every click, key and scroll, so it reads
    /// the executable path with one `proc_pidpath` call instead of listing
    /// running apps and spawning `ps`.
    pub fn is_electron(pid: i32) -> bool {
        crate::apps::path_of_pid(pid).is_some_and(|exe| ships_electron(std::path::Path::new(&exe)))
    }

    /// Evaluate JavaScript in the Electron app's main (Node.js) process at
    /// `pid` through its inspector, opening it with SIGUSR1 when no inspector
    /// is listening yet. Apps built with the inspector fuse off refuse.
    pub async fn execute_main(javascript: &str, pid: i32) -> anyhow::Result<String> {
        let existing_ports = listening_ports(pid).await;
        for &port in &existing_ports {
            if CdpClient::is_node_inspector(port).await {
                return CdpClient::evaluate_node(javascript, port).await;
            }
        }
        unsafe {
            libc::kill(pid, libc::SIGUSR1);
        }
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            for port in listening_ports(pid).await {
                if !existing_ports.contains(&port) && CdpClient::is_node_inspector(port).await {
                    return CdpClient::evaluate_node(javascript, port).await;
                }
            }
        }
        anyhow::bail!("no Node inspector for Electron pid {pid} (the app may disable it)")
    }

    /// Evaluate JavaScript in the Electron app at `pid` via CDP.
    pub async fn execute(javascript: &str, pid: i32) -> anyhow::Result<String> {
        // Only inspect listeners owned by the requested process. Probing common
        // CDP ports globally can execute JavaScript in an unrelated browser.
        let existing_ports = listening_ports(pid).await;
        for &port in &existing_ports {
            if CdpClient::is_available(port).await {
                return CdpClient::evaluate(javascript, port).await;
            }
        }

        // Snapshot current ports, then send SIGUSR1 to open the inspector.
        let before_ports: HashSet<u16> = existing_ports.iter().cloned().collect();

        unsafe {
            libc::kill(pid, libc::SIGUSR1);
        }

        // Poll for a new process-owned port.
        let mut new_port: Option<u16> = None;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let current = listening_ports(pid).await;
            for &p in &current {
                if !before_ports.contains(&p) && CdpClient::is_available(p).await {
                    new_port = Some(p);
                    break;
                }
            }
            if new_port.is_some() {
                break;
            }
        }

        let port = new_port.ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find Electron inspector port for pid {pid}. \
                             Try launching with --inspect or --remote-debugging-port."
            )
        })?;

        CdpClient::evaluate(javascript, port).await
    }
}

/// Whether any `.app` bundle containing `exe` ships the Electron framework.
fn ships_electron(exe: &std::path::Path) -> bool {
    exe.ancestors()
        .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .any(|bundle| {
            bundle
                .join("Contents/Frameworks/Electron Framework.framework")
                .exists()
        })
}

#[cfg(test)]
mod electron_detection_tests {
    use super::ships_electron;

    #[test]
    fn finds_the_framework_in_the_bundle_that_contains_the_executable() {
        let root = std::env::temp_dir().join(format!("electron-detect-{}", std::process::id()));
        let app = root.join("Chat.app/Contents");
        std::fs::create_dir_all(app.join("MacOS")).unwrap();
        std::fs::create_dir_all(app.join("Frameworks/Electron Framework.framework")).unwrap();
        let helper = app.join("Frameworks/Chat Helper.app/Contents/MacOS");
        std::fs::create_dir_all(&helper).unwrap();
        let native = root.join("Notes.app/Contents/MacOS");
        std::fs::create_dir_all(&native).unwrap();

        assert!(ships_electron(&app.join("MacOS/Chat")));
        assert!(ships_electron(&helper.join("Chat Helper")));
        assert!(!ships_electron(&native.join("Notes")));
        assert!(!ships_electron(std::path::Path::new("/usr/bin/true")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
