use std::path::PathBuf;

use ant_core::node::binary::NoopProgress;
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::{AddNodeOpts, BinarySource, PortRange};

/// Create a fake binary that responds to --version
fn create_fake_binary(dir: &std::path::Path) -> PathBuf {
    let binary_path = dir.join("fake-antnode");
    #[cfg(unix)]
    {
        std::fs::write(&binary_path, "#!/bin/sh\necho \"antnode 0.1.0-test\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        std::fs::write(&binary_path, "@echo antnode 0.1.0-test\n").unwrap();
    }
    binary_path
}

#[tokio::test]
async fn add_nodes_then_reset_clears_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = create_fake_binary(tmp.path());
    let reg_path = tmp.path().join("registry.json");

    // Add 3 nodes with ports
    let opts = AddNodeOpts {
        count: 3,
        rewards_address: "0xreset_integration".to_string(),
        node_port: Some(PortRange::Range(12000, 12002)),
        metrics_port: Some(PortRange::Range(13000, 13002)),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        ..Default::default()
    };

    let add_result = ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();
    assert_eq!(add_result.nodes_added.len(), 3);

    // Verify registry has 3 nodes with next_id = 4
    let reg = NodeRegistry::load(&reg_path).unwrap();
    assert_eq!(reg.len(), 3);
    assert_eq!(reg.next_id, 4);

    // Verify all data and log directories exist
    for node in &add_result.nodes_added {
        assert!(node.data_dir.exists(), "data dir should exist before reset");
        assert!(
            node.log_dir.as_ref().unwrap().exists(),
            "log dir should exist before reset"
        );
        assert!(
            node.binary_path.exists(),
            "binary should exist before reset"
        );
    }

    // Reset
    let reset_result = ant_core::node::reset(&reg_path).unwrap();

    // Verify result
    assert_eq!(reset_result.nodes_cleared, 3);
    assert_eq!(reset_result.data_dirs_removed.len(), 3);
    assert_eq!(reset_result.log_dirs_removed.len(), 3);

    // Verify all directories are gone
    for node in &add_result.nodes_added {
        assert!(
            !node.data_dir.exists(),
            "data dir should not exist after reset"
        );
        assert!(
            !node.log_dir.as_ref().unwrap().exists(),
            "log dir should not exist after reset"
        );
    }

    // Verify registry is empty and next_id reset to 1
    let reg = NodeRegistry::load(&reg_path).unwrap();
    assert!(reg.is_empty());
    assert_eq!(reg.next_id, 1);
}

#[tokio::test]
async fn reset_then_add_nodes_starts_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = create_fake_binary(tmp.path());
    let reg_path = tmp.path().join("registry.json");

    // Add 2 nodes
    let opts = AddNodeOpts {
        count: 2,
        rewards_address: "0xfirst_batch".to_string(),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary.clone()),
        ..Default::default()
    };

    ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();

    // Reset
    ant_core::node::reset(&reg_path).unwrap();

    // Add new nodes — IDs should start from 1 again
    let opts = AddNodeOpts {
        count: 1,
        rewards_address: "0xsecond_batch".to_string(),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        ..Default::default()
    };

    let result = ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();

    assert_eq!(result.nodes_added.len(), 1);
    assert_eq!(
        result.nodes_added[0].id, 1,
        "ID should restart from 1 after reset"
    );
    assert_eq!(result.nodes_added[0].rewards_address, "0xsecond_batch");
}

#[test]
fn reset_empty_registry_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let reg_path = tmp.path().join("registry.json");

    let result = ant_core::node::reset(&reg_path).unwrap();
    assert_eq!(result.nodes_cleared, 0);
    assert!(result.data_dirs_removed.is_empty());
    assert!(result.log_dirs_removed.is_empty());

    // Registry should still be valid
    let reg = NodeRegistry::load(&reg_path).unwrap();
    assert!(reg.is_empty());
    assert_eq!(reg.next_id, 1);
}
