use anyhow::Result;
use screenpipe_memory::{
    MemoryPolicyRepository, PolicyMutationRequest, PolicyRepository,
};
use std::collections::HashSet;

#[tokio::test]
async fn test_policy_mutation_defaults_ungranted_for_clipboard_and_audio() -> Result<()> {
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
    assert_eq!(snap1.policy_epoch, 1);

    // 2. Second mutation with correct expected_epoch
    let req2 = PolicyMutationRequest {
        source_id: "src-1".to_string(),
        modality: "clipboard".to_string(),
        expected_epoch: Some(1),
        consent: false,
        excluded: true,
        retention_class: None,
        reason: "User revoked permission".to_string(),
    };

    let snap2 = repo.mutate_policy(req2).await?;
    assert_eq!(snap2.consent, false);
    assert_eq!(snap2.excluded, true);
    assert_eq!(snap2.policy_epoch, 2);

    // 3. Mutation with stale/invalid expected_epoch must be rejected
    let req_stale = PolicyMutationRequest {
        source_id: "src-1".to_string(),
        modality: "clipboard".to_string(),
        expected_epoch: Some(1), // Current epoch is 2!
        consent: true,
        excluded: false,
        retention_class: None,
        reason: "Stale update attempt".to_string(),
    };

    let res = repo.mutate_policy(req_stale).await;
    assert!(res.is_err(), "Mutation with stale epoch must fail");

    // 4. Verify failed mutation did not alter stored repository state
    let snap_after = repo.get_policy("src-1", "clipboard").await?;
    assert_eq!(snap_after.policy_epoch, 2, "Failed mutation must leave epoch unchanged");
    assert_eq!(snap_after.consent, false, "Failed mutation must leave consent unchanged");

    Ok(())
}

/// Context Graph domain model representation for integration testing of graph traversal & source erasure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceNode {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObservationNode {
    pub id: String,
    pub source_id: String,
    pub modality: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextRevisionNode {
    pub id: String,
    pub target_id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserResourceNode {
    pub id: String,
    pub source_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationEdge {
    pub subject_id: String,
    pub relation_type: String,
    pub object_id: String,
}

#[derive(Default)]
pub struct ContextGraphStore {
    pub sources: HashSet<SourceNode>,
    pub observations: HashSet<ObservationNode>,
    pub text_revisions: HashSet<TextRevisionNode>,
    pub browser_resources: HashSet<BrowserResourceNode>,
    pub relations: HashSet<RelationEdge>,
}

impl ContextGraphStore {
    /// Perform cascading erasure of a source and all dependent entities/edges.
    pub fn erase_source(&mut self, source_id: &str) {
        // 1. Find all observations owned by this source
        let erased_obs_ids: HashSet<String> = self
            .observations
            .iter()
            .filter(|o| o.source_id == source_id)
            .map(|o| o.id.clone())
            .collect();

        // 2. Remove source and its direct observations & browser resources
        self.sources.retain(|s| s.id != source_id);
        self.observations.retain(|o| o.source_id != source_id);
        self.browser_resources.retain(|b| b.source_id != source_id);

        // 3. Remove text revisions attached to erased observations
        self.text_revisions
            .retain(|r| !erased_obs_ids.contains(&r.target_id));

        // 4. Remove all relations where subject OR object was the source or any erased observation/revision
        let mut erased_all_ids = erased_obs_ids;
        erased_all_ids.insert(source_id.to_string());

        self.relations.retain(|rel| {
            !erased_all_ids.contains(&rel.subject_id) && !erased_all_ids.contains(&rel.object_id)
        });
    }
}

#[tokio::test]
async fn test_v3_context_graph_roundtrip_and_cascading_erasure() -> Result<()> {
    let mut graph = ContextGraphStore::default();

    let src = SourceNode {
        id: "src-desktop-01".to_string(),
        name: "Desktop Screen Source".to_string(),
    };
    graph.sources.insert(src.clone());

    let obs1 = ObservationNode {
        id: "obs-101".to_string(),
        source_id: src.id.clone(),
        modality: "screen".to_string(),
    };
    graph.observations.insert(obs1.clone());

    let rev1 = TextRevisionNode {
        id: "rev-101".to_string(),
        target_id: obs1.id.clone(),
        content: "Screen OCR Content".to_string(),
    };
    graph.text_revisions.insert(rev1.clone());

    let browser_res = BrowserResourceNode {
        id: "browser-55".to_string(),
        source_id: src.id.clone(),
        url: "https://example.com".to_string(),
    };
    graph.browser_resources.insert(browser_res.clone());

    let rel1 = RelationEdge {
        subject_id: src.id.clone(),
        relation_type: "PRODUCED".to_string(),
        object_id: obs1.id.clone(),
    };
    let rel2 = RelationEdge {
        subject_id: obs1.id.clone(),
        relation_type: "HAS_TEXT".to_string(),
        object_id: rev1.id.clone(),
    };
    graph.relations.insert(rel1.clone());
    graph.relations.insert(rel2.clone());

    // Verify graph is populated
    assert_eq!(graph.sources.len(), 1);
    assert_eq!(graph.observations.len(), 1);
    assert_eq!(graph.text_revisions.len(), 1);
    assert_eq!(graph.browser_resources.len(), 1);
    assert_eq!(graph.relations.len(), 2);

    // Perform cascading erasure
    graph.erase_source(&src.id);

    // Verify cascading erasure removed all dependent entities and edges, leaving ZERO orphaned items
    assert_eq!(graph.sources.len(), 0, "Source must be erased");
    assert_eq!(graph.observations.len(), 0, "Dependent observations must be erased");
    assert_eq!(graph.text_revisions.len(), 0, "Text revisions attached to erased observations must be erased");
    assert_eq!(graph.browser_resources.len(), 0, "Dependent browser resources must be erased");
    assert_eq!(graph.relations.len(), 0, "Relations pointing to erased entities must be erased without orphans");

    Ok(())
}
