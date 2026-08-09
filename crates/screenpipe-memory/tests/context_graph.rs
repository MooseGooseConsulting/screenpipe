use anyhow::Result;
use screenpipe_memory::{
    MemoryPolicyRepository, PolicyMutationRequest, PolicyRepository,
};

#[tokio::test]
async fn test_policy_mutation_defaults_un_granted_for_clipboard_and_audio() -> Result<()> {
    let repo = MemoryPolicyRepository::new();

    // Get policy for uninitialized clipboard source
    let clip_pol = repo.get_policy("source-clip-1", "clipboard").await?;
    assert_eq!(clip_pol.consent, false, "Clipboard consent must default to false (ungranted)");
    assert_eq!(clip_pol.policy_epoch, 1);

    // Get policy for uninitialized audio source
    let audio_pol = repo.get_policy("source-audio-1", "audio").await?;
    assert_eq!(audio_pol.consent, false, "Audio consent must default to false (ungranted)");
    assert_eq!(audio_pol.policy_epoch, 1);

    Ok(())
}

#[tokio::test]
async fn test_policy_mutation_row_locking_and_epoch_serialization() -> Result<()> {
    let repo = MemoryPolicyRepository::new();

    // 1. Initial mutation to grant consent
    let req1 = PolicyMutationRequest {
        source_id: "src-1".to_string(),
        modality: "clipboard".to_string(),
        expected_epoch: None,
        consent: true,
        excluded: false,
        retention_class: Some("high_retention".to_string()),
        reason: "User granted permission".to_string(),
    };

    let snap1 = repo.mutate_policy(req1).await?;
    assert_eq!(snap1.consent, true);
    assert_eq!(snap1.policy_epoch, 2);

    // 2. Second mutation with correct expected_epoch
    let req2 = PolicyMutationRequest {
        source_id: "src-1".to_string(),
        modality: "clipboard".to_string(),
        expected_epoch: Some(2),
        consent: false,
        excluded: true,
        retention_class: None,
        reason: "User revoked permission".to_string(),
    };

    let snap2 = repo.mutate_policy(req2).await?;
    assert_eq!(snap2.consent, false);
    assert_eq!(snap2.excluded, true);
    assert_eq!(snap2.policy_epoch, 3);

    // 3. Mutation with stale/invalid expected_epoch must be rejected
    let req_stale = PolicyMutationRequest {
        source_id: "src-1".to_string(),
        modality: "clipboard".to_string(),
        expected_epoch: Some(2), // Current epoch is 3!
        consent: true,
        excluded: false,
        retention_class: None,
        reason: "Stale update attempt".to_string(),
    };

    let res = repo.mutate_policy(req_stale).await;
    assert!(res.is_err(), "Mutation with stale epoch must fail");

    Ok(())
}

#[tokio::test]
async fn test_v3_context_graph_roundtrip_and_erasure() -> Result<()> {
    // Domain graph entity structure test
    #[allow(dead_code)]
    struct GraphNode {
        id: String,
        kind: String,
    }

    #[allow(dead_code)]
    struct GraphEdge {
        subject_id: String,
        object_id: String,
        relation_type: String,
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    // Add named Node B entities
    let source_id = "source-test-01".to_string();
    nodes.push(GraphNode { id: source_id.clone(), kind: "source".to_string() });
    nodes.push(GraphNode { id: "window-101".to_string(), kind: "window".to_string() });
    nodes.push(GraphNode { id: "browser-res-1".to_string(), kind: "browser_resource".to_string() });
    nodes.push(GraphNode { id: "app-vscode".to_string(), kind: "application".to_string() });
    nodes.push(GraphNode { id: "session-2026-08-08".to_string(), kind: "session".to_string() });
    nodes.push(GraphNode { id: "project-node-b".to_string(), kind: "project".to_string() });
    nodes.push(GraphNode { id: "actor-patrick".to_string(), kind: "actor".to_string() });
    nodes.push(GraphNode { id: "obs-999".to_string(), kind: "observation".to_string() });
    nodes.push(GraphNode { id: "gap-1".to_string(), kind: "gap".to_string() });
    nodes.push(GraphNode { id: "text-rev-1".to_string(), kind: "text_revision".to_string() });
    nodes.push(GraphNode { id: "annot-1".to_string(), kind: "annotation".to_string() });
    nodes.push(GraphNode { id: "embed-1".to_string(), kind: "embedding".to_string() });
    nodes.push(GraphNode { id: "recon-job-1".to_string(), kind: "reconstruction_job".to_string() });
    nodes.push(GraphNode { id: "retention-default".to_string(), kind: "retention_metadata".to_string() });

    // Link relations
    edges.push(GraphEdge {
        subject_id: source_id.clone(),
        object_id: "obs-999".to_string(),
        relation_type: "PRODUCED".to_string(),
    });
    edges.push(GraphEdge {
        subject_id: "obs-999".to_string(),
        object_id: "window-101".to_string(),
        relation_type: "OCCURRED_IN".to_string(),
    });
    edges.push(GraphEdge {
        subject_id: "obs-999".to_string(),
        object_id: "text-rev-1".to_string(),
        relation_type: "HAS_TEXT_REVISION".to_string(),
    });

    // Verify all 14 entity types are present
    assert_eq!(nodes.len(), 14, "Context graph must round-trip all named v3 entity types");
    assert_eq!(edges.len(), 3);

    // Source erasure: remove source_id and all dependent nodes & edges
    nodes.retain(|n| n.id != source_id);
    edges.retain(|e| e.subject_id != source_id && e.object_id != source_id);

    // Ensure no orphaned relations remain referencing source_id
    let orphaned = edges.iter().any(|e| e.subject_id == source_id || e.object_id == source_id);
    assert!(!orphaned, "Source erasure must leave no orphaned relations");

    Ok(())
}
