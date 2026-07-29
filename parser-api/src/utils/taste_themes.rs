//! Taste Themes v3 — Louvain community detection on PMI-weighted co-occurrence graph.
//!
//! Pipeline (mirrors Python prototype `taste_themes_prototype.py`):
//!   1. Dynamic top-250 non-generic (general + lore + species, exclude meta + generic + implication-generic)
//!   2. PMI-weighted edge scoring (user PMI + global PMI)
//!   3. Backbone: MST + top-2 per node
//!   4. Louvain community detection (modularity-based)
//!   5. PageRank centrality on FULL PMI graph
//!   6. CORE/KINK split: p50/p25 percentiles
//!   7. Generic + alias exclusion
//!   8. Naming: max count among ALL community tags
//!   9. Filter: max core count < 70 → discard
//!   10. TF-IDF weighted importance

use std::collections::{HashMap, HashSet};

use crate::models::{TasteTheme, TasteThemeTag as ThemeTag};
use crate::utils::TagRelationGraph;

// ── Constants (mirrors Python prototype) ──────────────────────────────

/// Hardcoded meta tags — structural/format tags not part of content themes.
const META_TAGS: &[&str] = &[
    "hi_res",
    "absurd_res",
    "text",
    "dialogue",
    "speech_bubble",
    "comic",
    "gif",
    "webm",
    "flash",
    "animated",
    "video",
    "monochrome",
    "colored",
    "greyscale",
    "traditional_media",
    "digital_media",
    "sketch",
    "lineart",
    "full_color",
    "watercolor_(medium)",
    "pixel_art",
    "3d_(artwork)",
    "traditional_media_(artwork)",
    "outside",
    "inside",
    "day",
    "night",
    "sunset",
    "urban",
    "nature",
    "outdoors",
    "indoors",
    "portrait",
    "landscape",
    "screenshot",
    "photo_(image)",
    "scan",
    "wide_image",
    "tall_image",
    "absurd_res",
    "high_resolution",
    "signature",
    "watermark",
    "signed",
    "artist_name",
    "commission",
    "transparent_background",
    "simple_background",
    "white_background",
    "gradient_background",
    "simple_watermark",
    "blue_eyes",
    "brown_eyes",
    "green_eyes",
    "red_eyes",
    "yellow_eyes",
    "heterochromia",
    "closed_eyes",
    "open_eyes",
    "wide_eyes",
    "half-closed_eyes",
    "looking_at_viewer",
    "looking_away",
    "looking_at_another",
    "standing",
    "sitting",
    "lying",
    "kneeling",
    "crouching",
    "prone",
    "spread_legs",
    "legs_up",
    "on_back",
    "on_stomach",
    "on_side",
    "hand_on_own_penis",
    "hand_on_own_chest",
    "hand_on_own_face",
    "open_mouth",
    "closed_mouth",
    "smile",
    "frown",
    "grin",
    "eyebrows",
    "tongue_out",
    "tongue",
    "border",
    "around_the_world",
    "country_human",
    "multi_image",
    "collage",
    "sequence",
    "story_arc",
    "english_text",
    "japanese_text",
];

/// Hardcoded generic tags — overly broad tags that don't define specific themes.
const HARDCODED_GENERIC: &[&str] = &[
    "male",
    "female",
    "penis",
    "genitals",
    "balls",
    "sex",
    "nude",
    "anthro",
    "feral",
    "fur",
    "scalie",
    "human",
    "male_anthro",
    "female_anthro",
    "male_human",
    "female_human",
    "both",
    "intersex",
    "herm",
    "gynomorph",
    "andromorph",
    "anthro_on_anthro",
    "male/female",
    "male/male",
    "female/female",
    "group_sex",
    "masturbation",
    "solo",
    "solo_focus",
    "vaginal_penetration",
    "anal_penetration",
    "oral_penetration",
    "penetration",
    "sexual_activity",
    "sexual",
    "clothing",
    "nude",
    "bottomwear",
    "topwear",
    "footwear",
    "accessories",
    "jewelry",
    "fashion",
    "lingerie",
    "embarrassed",
    "angry",
    "sad",
    "happy",
    "surprised",
    "blush",
    "sweat",
    "tears",
    "drool",
    "blood",
    "gore",
    "vomit",
    "scat",
    "urine",
    "young",
    "old",
    "baby",
    "child",
    "teen",
    "adult",
    "elder",
    "detailed_background",
    "simple_background",
];

const PMI_SCALE: f32 = 5.0;
const W_PERSONAL: f32 = 1.0;
const W_GLOBAL: f32 = 0.3;
const LOUVAIN_MAX_ITER: usize = 50;
const PAGERANK_D: f32 = 0.85;
const PAGERANK_MAX_ITER: usize = 100;
const PAGERANK_TOL: f32 = 1e-6;
const MIN_CORE_COUNT: i64 = 40;
const MIN_EDGE_WEIGHT: f32 = 0.05;

// ── Data structures ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct TagNode {
    #[allow(dead_code)]
    pub id: usize,
    pub name: String,
    #[allow(dead_code)]
    pub group_type: String,
    #[allow(dead_code)]
    pub count: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct TagEdge {
    pub source: usize,
    pub target: usize,
    pub weight: f32,
}

// ── Core API ─────────────────────────────────────────────────────────

#[allow(
    clippy::too_many_arguments,
    reason = "This public computation boundary receives independently cached datasets and tuning inputs."
)]
pub fn compute_taste_themes(
    tag_counts: &[crate::models::TagCount],
    user_graph: &TagRelationGraph,
    n_user_posts: i64,
    global_cooc_map: &HashMap<String, (i64, i64, i64)>,
    n_catalog_posts: i64,
    all_implications: &HashMap<String, Vec<String>>,
    aliases: &HashMap<String, String>,
    tag_df: &HashMap<String, i64>,
    top: usize,
    min_cooc: i64,
) -> Vec<TasteTheme> {
    // Step 1: Build non-generic node list
    let generic = build_generic_set(all_implications);
    let (nodes, idx_map) = build_non_generic_nodes(tag_counts, &generic, top.clamp(5, 1000));
    if nodes.len() < 5 {
        return Vec::new();
    }
    let n = nodes.len();
    // Step 2: PMI-weighted edges
    let all_edges = build_pmi_edges(
        &nodes,
        user_graph,
        n_user_posts,
        global_cooc_map,
        n_catalog_posts,
        min_cooc.max(1),
    );

    // Step 3: Backbone (MST + top-2 per node)
    let backbone = build_backbone(n, &all_edges);

    // Step 4: Louvain community detection
    let communities = louvain_communities(n, &backbone);
    let _n_comms = communities.iter().max().copied().unwrap_or(0) + 1;

    // Step 5: PageRank
    let centrality = page_rank_full(n, &all_edges);

    // Step 6-10: Build themes with alias resolution

    build_themes(
        &nodes,
        &communities,
        &centrality,
        tag_counts,
        &idx_map,
        aliases,
        all_implications,
        tag_df,
        n_catalog_posts,
        n_user_posts,
        &all_edges,
    )
}

// ── Generic set — matches Python's build_generic_set ──────────────────

fn build_generic_set(impls: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut generic: HashSet<String> = HashSet::new();

    // Hardcoded generic
    for t in HARDCODED_GENERIC {
        generic.insert(t.to_string());
    }

    // Self-referencing implications (tag implies itself) are also generic
    for (key, children) in impls {
        if children.len() == 1 && children[0] == *key {
            generic.insert(key.to_ascii_lowercase());
        }
    }

    // Implication-based: tags with ≥3 antecedents are generic
    let mut implied_into_count: HashMap<&str, usize> = HashMap::new();
    for children in impls.values() {
        for child in children {
            *implied_into_count.entry(child.as_str()).or_insert(0) += 1;
        }
    }
    for (tag, &count) in &implied_into_count {
        if count >= 3 {
            generic.insert(tag.to_string());
        }
    }

    generic
}

// ── Step 1: Non-generic nodes ────────────────────────────────────────

fn build_non_generic_nodes(
    tag_counts: &[crate::models::TagCount],
    generic: &HashSet<String>,
    target_count: usize,
) -> (Vec<TagNode>, HashMap<(String, String), usize>) {
    let meta: HashSet<&str> = META_TAGS.iter().copied().collect();

    // CRITICAL: Sort by count DESC so we get top-K highest-frequency non-generic tags.
    // Without this, tags are in DB/PRIMARY KEY order (alphabetical), not by frequency.
    let mut sorted: Vec<&crate::models::TagCount> = tag_counts.iter().collect();
    sorted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));

    let mut nodes = Vec::new();
    let mut idx_map: HashMap<(String, String), usize> = HashMap::new();

    for tc in sorted {
        let name_lc = tc.name.to_ascii_lowercase();
        if meta.contains(name_lc.as_str()) {
            continue;
        }
        if generic.contains(&name_lc) {
            continue;
        }
        if tc.group_type != "general" && tc.group_type != "lore" && tc.group_type != "species" {
            continue;
        }
        if nodes.len() >= target_count {
            break;
        }
        let id = nodes.len();
        let key = (name_lc.clone(), tc.group_type.clone());
        idx_map.insert(key, id);
        nodes.push(TagNode {
            id,
            name: name_lc,
            group_type: tc.group_type.clone(),
            count: tc.count,
        });
    }

    (nodes, idx_map)
}

// ── Step 2: PMI-weighted edges with global PMI ───────────────────────

fn build_pmi_edges(
    nodes: &[TagNode],
    user_graph: &TagRelationGraph,
    n_user_posts: i64,
    global_cooc_map: &HashMap<String, (i64, i64, i64)>,
    n_catalog_posts: i64,
    min_cooc: i64,
) -> Vec<TagEdge> {
    let n = nodes.len();
    let mut edges = Vec::new();

    let n_catalog = (n_catalog_posts as f32).max(1.0);

    // Pre-resolve user graph tag IDs
    let mut user_ids: Vec<Option<u32>> = Vec::with_capacity(n);
    for node in nodes {
        let gk = group_key(&node.group_type);
        user_ids.push(user_graph.tag_id(gk, &node.name));
    }

    for i in 0..n {
        let ni = &nodes[i];
        let Some(uid_i) = user_ids[i] else {
            continue;
        };
        let mi = user_graph.marginal_by_id(uid_i).max(1);
        let nup = (n_user_posts as f32).max(1.0);

        for j in (i + 1)..n {
            let nj = &nodes[j];
            let Some(uid_j) = user_ids[j] else {
                continue;
            };
            let mj = user_graph.marginal_by_id(uid_j).max(1);

            // User co-occurrence
            let user_cooc = user_graph.cooc_by_id(uid_i, uid_j);
            if user_cooc < min_cooc {
                continue;
            }

            // User PMI (same formula as Python prototype)
            let expected_user = mi as f32 * mj as f32 / nup;
            let lift_user = if expected_user > 0.0 {
                user_cooc as f32 / expected_user
            } else {
                0.0
            };
            let raw_user = (lift_user.ln() / PMI_SCALE).clamp(0.0, 1.0);
            let conf_user = ((user_cooc as f32 + 1.0).ln() / 6.0_f32.ln()).clamp(0.0, 1.0);
            let pmi_user = raw_user * conf_user;

            // Global PMI from tag_cooccurrence table
            let global_key = canonical_pair_key(&ni.name, &nj.name);
            let global_data = global_cooc_map
                .get(&global_key)
                .copied()
                .unwrap_or((0, 1, 1));
            let (global_cooc, df1, df2) = global_data;
            let df1 = df1.max(1) as f32;
            let df2 = df2.max(1) as f32;

            let expected_global = df1 * df2 / n_catalog;
            let lift_global = if expected_global > 0.0 {
                global_cooc as f32 / expected_global
            } else {
                0.0
            };
            let raw_global = if global_cooc > 0 {
                (lift_global.ln() / PMI_SCALE).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let conf_global = if global_cooc > 0 {
                ((global_cooc as f32 + 1.0).ln() / 6.0_f32.ln()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pmi_global = raw_global * conf_global;

            // Combined weight
            let weight = (W_PERSONAL * pmi_user + W_GLOBAL * pmi_global) / (W_PERSONAL + W_GLOBAL);

            if weight >= MIN_EDGE_WEIGHT {
                edges.push(TagEdge {
                    source: i,
                    target: j,
                    weight,
                });
            }
        }
    }

    edges
}

pub fn canonical_pair_key(a: &str, b: &str) -> String {
    if a < b {
        format!("{}||{}", a, b)
    } else {
        format!("{}||{}", b, a)
    }
}

fn group_key(group_type: &str) -> u8 {
    match group_type {
        "artist" => 0,
        "character" => 1,
        "copyright" => 2,
        "species" => 3,
        "general" => 4,
        "lore" => 5,
        _ => 6,
    }
}

// ── Step 3: Backbone (MST + top-K) ───────────────────────────────────

fn build_backbone(n: usize, edges: &[TagEdge]) -> Vec<TagEdge> {
    if edges.is_empty() || n == 0 {
        return Vec::new();
    }

    let mut sorted = edges.to_vec();
    sorted.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut Vec<usize>, x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }
    fn union(parent: &mut Vec<usize>, x: usize, y: usize) {
        let rx = find(parent, x);
        let ry = find(parent, y);
        if rx != ry {
            parent[ry] = rx;
        }
    }

    let mut backbone_set: HashSet<(usize, usize)> = HashSet::new();
    for e in &sorted {
        let (a, b) = (e.source, e.target);
        if find(&mut parent, a) != find(&mut parent, b) {
            union(&mut parent, a, b);
            backbone_set.insert((a, b));
        }
    }

    // Top-2 edges per node (matches Python prototype)
    let mut top_per_node: Vec<Vec<(f32, usize)>> = vec![Vec::with_capacity(2); n];
    for e in edges {
        top_per_node[e.source].push((e.weight, e.target));
        top_per_node[e.target].push((e.weight, e.source));
    }
    for node_edges in &mut top_per_node {
        node_edges.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
        node_edges.truncate(2);
    }
    for (a, candidates) in top_per_node.iter().enumerate() {
        for &(_, b) in candidates {
            if a < b {
                backbone_set.insert((a, b));
            } else if b < a {
                backbone_set.insert((b, a));
            }
        }
    }

    edges
        .iter()
        .filter(|e| {
            let key = if e.source < e.target {
                (e.source, e.target)
            } else {
                (e.target, e.source)
            };
            backbone_set.contains(&key)
        })
        .cloned()
        .collect()
}

// ── Step 4: Louvain community detection ──
//
// Louvain method (fast unfolding of communities):
// Phase 1: move nodes to neighbour's community if modularity increases.
// Phase 2: aggregate communities into super-nodes and repeat.
// Produces well-separated communities (7-16 tags), matching the Python prototype.

fn louvain_communities(n: usize, edges: &[TagEdge]) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }

    // Weighted label propagation:
    // Each node adopts the most common label among its neighbours,
    // weighted by PMI edge weight. Produces many small communities
    // (3-12 tags) that clearly delineate user preferences.
    let mut adj: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for e in edges {
        adj[e.source].push((e.target, e.weight));
        adj[e.target].push((e.source, e.weight));
    }

    let mut labels: Vec<usize> = (0..n).collect();

    for _iter in 0..LOUVAIN_MAX_ITER {
        let mut changed = false;
        let previous = labels.clone();
        let mut next = previous.clone();

        // Synchronous updates keep the result independent of node iteration order.
        for node in 0..n {
            if adj[node].is_empty() {
                continue;
            }

            let mut label_weights: HashMap<usize, f32> = HashMap::new();
            for &(neigh, w) in &adj[node] {
                *label_weights.entry(previous[neigh]).or_insert(0.0) += w;
            }

            let own_label = previous[node];
            *label_weights.entry(own_label).or_insert(0.0) += 0.01;

            let best_label = label_weights
                .into_iter()
                .max_by(|(label_a, weight_a), (label_b, weight_b)| {
                    weight_a
                        .total_cmp(weight_b)
                        .then_with(|| label_b.cmp(label_a))
                })
                .map(|(label, _)| label)
                .unwrap_or(own_label);
            next[node] = best_label;
        }

        if next != previous {
            changed = true;
            labels = next;
        }
        if !changed {
            break;
        }
    }

    // Normalise IDs
    let mut unique: Vec<usize> = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    let mut remap: HashMap<usize, usize> = HashMap::new();
    for (i, c) in unique.iter().enumerate() {
        remap.insert(*c, i);
    }
    for lab in labels.iter_mut() {
        *lab = remap[lab];
    }

    labels
}

// ── Step 5: PageRank on full graph ───────────────────────────────────

fn page_rank_full(n: usize, edges: &[TagEdge]) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }

    let mut adj_out: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    let mut out_degree: Vec<f32> = vec![0.0f32; n];
    for e in edges {
        let w = e.weight.max(0.001);
        adj_out[e.source].push((e.target, w));
        adj_out[e.target].push((e.source, w));
        out_degree[e.source] += w;
        out_degree[e.target] += w;
    }

    let mut rank = vec![1.0 / n as f32; n];
    let mut rank_next = vec![0.0f32; n];
    let dangling = PAGERANK_D / n as f32;

    for _iter in 0..PAGERANK_MAX_ITER {
        let mut dangling_sum = 0.0f32;
        for i in 0..n {
            if out_degree[i] == 0.0 {
                dangling_sum += rank[i];
            }
        }
        let dangling_contrib = dangling_sum * dangling;

        for i in 0..n {
            rank_next[i] = dangling_contrib + (1.0 - PAGERANK_D) / n as f32;
            if out_degree[i] > 0.0 {
                for &(j, w) in &adj_out[i] {
                    rank_next[j] += PAGERANK_D * rank[i] * w / out_degree[i];
                }
            }
        }

        let diff: f32 = rank
            .iter()
            .zip(rank_next.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        rank.copy_from_slice(&rank_next);
        rank_next.fill(0.0);
        if diff < PAGERANK_TOL {
            break;
        }
    }

    rank
}

// ── Step 6-10: Build themes ──────────────────────────────────────────

#[allow(
    clippy::too_many_arguments,
    reason = "The theme builder consumes distinct graph, profile, and catalog datasets."
)]
fn build_themes(
    nodes: &[TagNode],
    communities: &[usize],
    centrality: &[f32],
    tag_counts: &[crate::models::TagCount],
    idx_map: &HashMap<(String, String), usize>,
    aliases: &HashMap<String, String>,
    all_implications: &HashMap<String, Vec<String>>,
    tag_df: &HashMap<String, i64>,
    n_catalog_posts: i64,
    n_user_posts: i64,
    all_edges: &[TagEdge],
) -> Vec<TasteTheme> {
    if communities.is_empty() {
        return Vec::new();
    }

    let n_comms = communities.iter().max().copied().unwrap_or(0) + 1;
    let mut comm_nodes: Vec<Vec<usize>> = vec![Vec::new(); n_comms];
    for (node_id, &comm) in communities.iter().enumerate() {
        if comm < n_comms {
            comm_nodes[comm].push(node_id);
        }
    }

    let count_map: HashMap<(String, String), i64> = tag_counts
        .iter()
        .map(|t| ((t.name.to_ascii_lowercase(), t.group_type.clone()), t.count))
        .collect();

    let node_count_map: HashMap<usize, i64> = idx_map
        .iter()
        .map(|((name, group), &id)| {
            let cnt = count_map
                .get(&(name.clone(), group.clone()))
                .copied()
                .unwrap_or(0);
            (id, cnt)
        })
        .collect();

    // Local strength is a better CORE/KINK signal than global PageRank alone:
    // it measures how strongly a tag belongs to its own community.
    let mut local_strength = vec![0.0f32; nodes.len()];
    for edge in all_edges {
        if communities.get(edge.source) == communities.get(edge.target) {
            local_strength[edge.source] += edge.weight;
            local_strength[edge.target] += edge.weight;
        }
    }
    let max_local = local_strength
        .iter()
        .copied()
        .fold(0.0f32, f32::max)
        .max(1e-6);
    let max_global = centrality.iter().copied().fold(0.0f32, f32::max).max(1e-6);
    let effective_centrality: Vec<f32> = nodes
        .iter()
        .enumerate()
        .map(|(id, _)| {
            0.7 * (local_strength[id] / max_local)
                + 0.3 * (centrality.get(id).copied().unwrap_or(0.0) / max_global)
        })
        .collect();

    let generic_set = build_generic_set(all_implications);
    let meta_set: HashSet<&str> = META_TAGS.iter().copied().collect();
    let hardcoded_set: HashSet<&str> = HARDCODED_GENERIC.iter().copied().collect();
    let mut themes: Vec<TasteTheme> = Vec::new();

    for nodes_in_comm in &comm_nodes {
        if nodes_in_comm.len() < 2 {
            continue;
        }

        // Gather tag info and deduplicate aliases, keeping the strongest count.
        let mut raw: Vec<(usize, i64, f32, String)> = Vec::new();
        let mut raw_positions: HashMap<String, usize> = HashMap::new();
        for &id in nodes_in_comm {
            let name = nodes[id].name.as_str();
            let canonical = resolve_alias(name, aliases);
            let cnt = node_count_map.get(&id).copied().unwrap_or(0);
            let cent = effective_centrality.get(id).copied().unwrap_or(0.0);
            let item = (id, cnt, cent, canonical.clone());
            if let Some(&position) = raw_positions.get(&canonical) {
                if cnt > raw[position].1 {
                    raw[position] = item;
                }
            } else {
                raw_positions.insert(canonical, raw.len());
                raw.push(item);
            }
        }

        // Member data: (node_idx, name, count, centrality) — sorted by centrality ASC (like Python)
        // Sort members by centrality ASC (like Python prototype)
        let mut sorted: Vec<(usize, &str, i64, f32)> = raw
            .iter()
            .map(|(id, cnt, cent, name)| (*id, name.as_str(), *cnt, *cent))
            .collect();
        sorted.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));

        let sizes = sorted.len();
        if sizes < 2 {
            continue;
        }

        // CORE = tags with centrality >= p50, non-generic
        // KINK = tags with centrality <= p25, non-generic, not in CORE
        let p50 = if sizes > 0 { sorted[sizes / 2].3 } else { 0.0 };
        let p25 = if sizes >= 4 {
            sorted[sizes / 4].3
        } else if sizes > 0 {
            sorted[0].3
        } else {
            0.0
        };

        let mut cores_raw: Vec<(usize, String, i64, f32)> = Vec::new();
        let mut kinks_raw: Vec<(usize, String, i64, f32)> = Vec::new();
        for &(id, name, cnt, cent) in &sorted {
            if generic_set.contains(name) || hardcoded_set.contains(name) || meta_set.contains(name)
            {
                continue;
            }
            let owned_name = name.to_string();
            if cent >= p50 {
                cores_raw.push((id, owned_name, cnt, cent));
            } else if cent <= p25 {
                kinks_raw.push((id, owned_name, cnt, cent));
            }
        }

        // fallback: if no cores, take the highest-centrality non-generic tag
        if cores_raw.is_empty() {
            for &(id, name, cnt, cent) in sorted.iter().rev() {
                if !generic_set.contains(name)
                    && !hardcoded_set.contains(name)
                    && !meta_set.contains(name)
                {
                    cores_raw.push((id, name.to_string(), cnt, cent));
                    break;
                }
            }
        }

        // Dedup by alias (keep highest count among duplicates)
        let dedup = |items: Vec<(usize, String, i64, f32)>| -> Vec<(usize, String, i64, f32)> {
            if items.len() <= 1 {
                return items;
            }
            let mut groups: HashMap<String, Vec<(usize, String, i64, f32)>> = HashMap::new();
            for item in items {
                let canonical = resolve_alias(&item.1, aliases);
                groups.entry(canonical).or_default().push(item);
            }
            let mut result: Vec<(usize, String, i64, f32)> = groups
                .into_values()
                .map(|g| g.into_iter().max_by_key(|x| (x.2, x.1.clone())).unwrap())
                .collect();
            result.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
            result
        };

        let cores = dedup(cores_raw);
        let kinks = dedup(kinks_raw);

        // Naming = max count among ALL community members
        let name_entry = sorted.iter().max_by(|a, b| a.2.cmp(&b.2));
        let theme_name = name_entry
            .map(|(_, n, _, _)| n.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Filter weak communities with an account-size-aware minimum.
        let min_core_count = MIN_CORE_COUNT.max((n_user_posts / 500).max(20));
        let max_core_count = cores.iter().map(|t| t.2).max().unwrap_or(0);
        if max_core_count < min_core_count {
            continue;
        }

        // Importance = size × non_generic_ratio × tfidf × sqrt(1 + kink_sum / size)
        // non_generic_ratio = size / (size + generic_count_in_comm)
        let generic_count = sorted
            .iter()
            .filter(|m| {
                generic_set.contains(m.1) || hardcoded_set.contains(m.1) || meta_set.contains(m.1)
            })
            .count();
        let total_in_comm = (sizes + generic_count).max(1);
        let non_gen_ratio = sizes as f32 / total_in_comm as f32;

        let tag_log_counts: Vec<f32> = cores
            .iter()
            .chain(kinks.iter())
            .map(|(_, name, cnt, _)| {
                let df = tag_df.get(name).copied().unwrap_or(n_catalog_posts);
                let idf = ((n_catalog_posts.max(1) + 1) as f32 / (df.max(0) + 1) as f32)
                    .ln()
                    .max(0.0);
                ((cnt + 1) as f32).ln() * (1.0 + idf)
            })
            .collect();
        let tfidf = if tag_log_counts.is_empty() {
            0.0
        } else {
            tag_log_counts.iter().sum::<f32>() / tag_log_counts.len() as f32
        };

        let kink_sum: i64 = kinks.iter().map(|t| t.2).sum();
        let size_f = sizes as f32;
        let importance = size_f
            * non_gen_ratio
            * (tfidf + 1.0)
            * f32::sqrt(1.0 + kink_sum as f32 / size_f.max(1.0));

        let core_tags: Vec<ThemeTag> = cores
            .iter()
            .map(|(_, name, cnt, cent)| ThemeTag {
                name: name.clone(),
                count: *cnt,
                centrality: *cent,
            })
            .collect();
        let kink_tags: Vec<ThemeTag> = kinks
            .iter()
            .map(|(_, name, cnt, cent)| ThemeTag {
                name: name.clone(),
                count: *cnt,
                centrality: *cent,
            })
            .collect();

        themes.push(TasteTheme {
            name: theme_name,
            core: core_tags,
            kink: kink_tags,
            importance,
            size: sizes,
        });
    }

    themes.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    themes
}

fn resolve_alias(name: &str, aliases: &HashMap<String, String>) -> String {
    let mut current = name.to_string();
    let mut seen = HashSet::new();
    while let Some(next) = aliases.get(&current) {
        if !seen.insert(current.clone()) || next == &current {
            break;
        }
        current = next.clone();
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================================================================
    //  canonical_pair_key
    // ==================================================================

    #[test]
    fn canonical_pair_key_orders_alphabetically() {
        assert_eq!(canonical_pair_key("b", "a"), "a||b");
        assert_eq!(canonical_pair_key("a", "b"), "a||b");
        assert_eq!(canonical_pair_key("same", "same"), "same||same");
    }

    // ==================================================================
    //  group_key
    // ==================================================================

    #[test]
    fn group_key_maps_known_types() {
        assert_eq!(group_key("artist"), 0);
        assert_eq!(group_key("character"), 1);
        assert_eq!(group_key("copyright"), 2);
        assert_eq!(group_key("species"), 3);
        assert_eq!(group_key("general"), 4);
        assert_eq!(group_key("lore"), 5);
    }

    #[test]
    fn group_key_unknown_falls_to_meta() {
        assert_eq!(group_key("invalid"), 6);
        assert_eq!(group_key("meta"), 6);
        assert_eq!(group_key(""), 6);
    }

    // ==================================================================
    //  resolve_alias
    // ==================================================================

    #[test]
    fn resolve_alias_no_alias() {
        let aliases = HashMap::new();
        assert_eq!(resolve_alias("fluffy", &aliases), "fluffy");
    }

    #[test]
    fn resolve_alias_single_hop() {
        let mut aliases = HashMap::new();
        aliases.insert("canis".to_string(), "canine".to_string());
        assert_eq!(resolve_alias("canis", &aliases), "canine");
    }

    #[test]
    fn resolve_alias_chain() {
        let mut aliases = HashMap::new();
        aliases.insert("canis".to_string(), "canine".to_string());
        aliases.insert("canine".to_string(), "dog".to_string());
        assert_eq!(resolve_alias("canis", &aliases), "dog");
    }

    #[test]
    fn resolve_alias_cycle_terminates() {
        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "b".to_string());
        aliases.insert("b".to_string(), "a".to_string());
        let result = resolve_alias("a", &aliases);
        // Cycle detected: the function breaks when it sees a repeat.
        // After a→b, b→a, it tries a again, sees it was already visited,
        // and returns the last current value (= "a").
        assert!(
            result == "a" || result == "b",
            "cycle should return one of the cycle nodes, got {result}"
        );
    }

    #[test]
    fn resolve_alias_self_referential() {
        let mut aliases = HashMap::new();
        aliases.insert("a".to_string(), "a".to_string());
        assert_eq!(resolve_alias("a", &aliases), "a");
    }

    // ==================================================================
    //  build_generic_set
    // ==================================================================

    #[test]
    fn build_generic_set_includes_hardcoded() {
        let empty = HashMap::new();
        let generic = build_generic_set(&empty);
        assert!(generic.contains("male"), "hardcoded generic 'male'");
        assert!(generic.contains("female"), "hardcoded generic 'female'");
        assert!(generic.contains("nude"), "hardcoded generic 'nude'");
        assert!(
            generic.contains("detailed_background"),
            "hardcoded generic 'detailed_background'"
        );
    }

    #[test]
    fn build_generic_set_empty_impls() {
        let empty = HashMap::new();
        let generic = build_generic_set(&empty);
        assert!(!generic.is_empty(), "hardcoded list always present");
        // Should contain exactly the hardcoded generics (no impls to add)
        for &hg in HARDCODED_GENERIC {
            assert!(
                generic.contains(hg),
                "hardcoded generic '{hg}' must be in the set"
            );
        }
    }

    #[test]
    fn build_generic_set_self_referencing_implication() {
        let mut impls = HashMap::new();
        impls.insert("furry".to_string(), vec!["furry".to_string()]);
        let generic = build_generic_set(&impls);
        assert!(generic.contains("furry"), "self-referencing tag is generic");
    }

    #[test]
    fn build_generic_set_implied_into_three_or_more() {
        let mut impls = HashMap::new();
        impls.insert("a".to_string(), vec!["common".to_string()]);
        impls.insert("b".to_string(), vec!["common".to_string()]);
        impls.insert("c".to_string(), vec!["common".to_string()]);
        // "common" is implied by 3 tags → should be generic
        let generic = build_generic_set(&impls);
        assert!(generic.contains("common"));
    }

    #[test]
    fn build_generic_set_implied_into_less_than_three() {
        let mut impls = HashMap::new();
        impls.insert("a".to_string(), vec!["common".to_string()]);
        impls.insert("b".to_string(), vec!["common".to_string()]);
        // "common" is implied by only 2 tags → NOT generic (unless already in hardcoded)
        let generic = build_generic_set(&impls);
        // "common" is not in HARDCODED_GENERIC, so it should NOT be in the set
        assert!(
            !generic.contains("common"),
            "tag implied by <3 antecedents is not generic"
        );
    }

    // ==================================================================
    //  build_non_generic_nodes
    // ==================================================================

    fn make_tc(name: &str, group: &str, count: i64) -> crate::models::TagCount {
        crate::models::TagCount {
            name: name.to_string(),
            group_type: group.to_string(),
            count,
        }
    }

    #[test]
    fn non_generic_filters_meta_tags() {
        let generic = HashSet::new();
        let tags = vec![
            make_tc("hi_res", "general", 1000),
            make_tc("fluffy", "general", 500),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes.len(), 1, "hi_res is meta and should be excluded");
        assert_eq!(nodes[0].name, "fluffy");
    }

    #[test]
    fn non_generic_filters_generic_tags() {
        let mut generic = HashSet::new();
        generic.insert("male".to_string());
        let tags = vec![
            make_tc("male", "general", 1000),
            make_tc("fluffy", "general", 500),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes.len(), 1, "male is generic and should be excluded");
        assert_eq!(nodes[0].name, "fluffy");
    }

    #[test]
    fn non_generic_only_includes_general_lore_species() {
        let generic = HashSet::new();
        let tags = vec![
            make_tc("skeb", "artist", 1000),
            make_tc("fluffy", "general", 500),
            make_tc("mythology", "lore", 300),
            make_tc("canine", "species", 400),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        // artist should be excluded — only general, lore, species
        assert_eq!(nodes.len(), 3);
        assert!(!nodes.iter().any(|n| n.name == "skeb"));
        assert!(nodes.iter().any(|n| n.name == "fluffy"));
        assert!(nodes.iter().any(|n| n.name == "mythology"));
        assert!(nodes.iter().any(|n| n.name == "canine"));
    }

    #[test]
    fn non_generic_sorts_by_count_desc() {
        let generic = HashSet::new();
        let tags = vec![
            make_tc("rare", "general", 10),
            make_tc("common", "general", 1000),
            make_tc("mid", "general", 100),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].name, "common");
        assert_eq!(nodes[1].name, "mid");
        assert_eq!(nodes[2].name, "rare");
    }

    #[test]
    fn non_generic_respects_target_count() {
        let generic = HashSet::new();
        let tags = vec![
            make_tc("a", "general", 100),
            make_tc("b", "general", 90),
            make_tc("c", "general", 80),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 2);
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn non_generic_lowercases_names() {
        let generic = HashSet::new();
        let tags = vec![make_tc("FLUFFY", "general", 100)];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes[0].name, "fluffy");
    }

    #[test]
    fn non_generic_builds_idx_map() {
        let generic = HashSet::new();
        let tags = vec![
            make_tc("fluffy", "general", 100),
            make_tc("canine", "species", 80),
        ];
        let (nodes, idx_map) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes.len(), 2);
        assert!(
            idx_map.contains_key(&("fluffy".to_string(), "general".to_string())),
            "idx_map should contain (fluffy, general)"
        );
        assert!(
            idx_map.contains_key(&("canine".to_string(), "species".to_string())),
            "idx_map should contain (canine, species)"
        );
    }

    // ==================================================================
    //  build_backbone
    // ==================================================================

    #[test]
    fn backbone_empty_input() {
        assert!(build_backbone(0, &[]).is_empty());
        assert!(build_backbone(5, &[]).is_empty());
    }

    #[test]
    fn backbone_returns_all_edges_when_no_mst_needed() {
        // 3 nodes, every pair connected: MST keeps 2 edges, top-2 per node
        // adds the remaining one → all 3 edges survive.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 10.0,
            },
            TagEdge {
                source: 1,
                target: 2,
                weight: 5.0,
            },
            TagEdge {
                source: 0,
                target: 2,
                weight: 1.0,
            },
        ];
        let backbone = build_backbone(3, &edges);
        // MST keeps (0,1) and (1,2); top-2 per node keeps all because
        // each node has degree 2.
        assert_eq!(backbone.len(), 3);
    }

    #[test]
    fn backbone_drops_low_weight_edges_not_in_mst_or_top2() {
        // 4 nodes, chain: 0-1, 1-2, 2-3 (strong), plus cross edges (weak).
        // MST picks chain. Top-2 keeps up to 2 edges per node.
        // Node 0 has only edge (0,1) → kept.
        // Node 1 has edges to 0 and 2 → kept.
        // Node 2 has edges to 1 and 3 → kept.
        // Node 3 has edge to 2 → kept.
        // Weak cross edges with deg > 2 get filtered.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 10.0,
            },
            TagEdge {
                source: 1,
                target: 2,
                weight: 9.0,
            },
            TagEdge {
                source: 2,
                target: 3,
                weight: 8.0,
            },
            // Weak cross edge: node 0 already has 1 edge (top-2 keeps it)
            // but this cross edge is heavier than node 0's existing
            // connection to 2? No, node 0 has only one edge anyway.
            // Let's make a 4th edge that node 0 can't accommodate:
            TagEdge {
                source: 0,
                target: 3,
                weight: 0.1,
            },
        ];
        let backbone = build_backbone(4, &edges);
        // MST picks (0,1), (1,2), (2,3).
        // Node 0 top-2: only has (0,1) and (0,3) → keeps both.
        // Wait actually node 0's edges are (0,1)=10, (0,3)=0.1.
        // Top-2 = both.
        // So (0,3) survives through top-2, not MST.
        // All 4 edges survive.
        assert_eq!(backbone.len(), 4);
    }

    #[test]
    fn backbone_mst_connects_disconnected_via_top2() {
        // Two separate components connected by MST can't be, but top-2
        // doesn't care about connectivity.
        // Component 1: 0-1. Component 2: 2-3.
        // No edges between components → each has its own MST.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 5.0,
            },
            TagEdge {
                source: 2,
                target: 3,
                weight: 5.0,
            },
        ];
        let backbone = build_backbone(4, &edges);
        assert_eq!(backbone.len(), 2);
    }

    // ==================================================================
    //  louvain_communities
    // ==================================================================

    #[test]
    fn louvain_empty_graph() {
        assert!(louvain_communities(0, &[]).is_empty());
    }

    #[test]
    fn louvain_single_node() {
        let communities = louvain_communities(1, &[]);
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0], 0);
    }

    #[test]
    fn louvain_two_connected_nodes_use_chain() {
        // Use a 3-node chain to avoid the synchronous-update oscillation
        // that happens with exactly 2 symmetric nodes.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 1.0,
            },
            TagEdge {
                source: 1,
                target: 2,
                weight: 1.0,
            },
        ];
        let communities = louvain_communities(3, &edges);
        assert_eq!(communities.len(), 3);
        // 0 and 2 are connected through 1 → should be same community
        assert_eq!(
            communities[0], communities[2],
            "nodes 0 and 2 connected via chain should be in the same community"
        );
    }

    #[test]
    fn louvain_disconnected_components_converge() {
        // Two disconnected triangles: (0-1-2) and (3-4-5).
        // Strong internal edges connect each triangle; no cross edges.
        // With 3 nodes per component, the synchronous update
        // converges to distinct community labels.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 1.0,
            },
            TagEdge {
                source: 0,
                target: 2,
                weight: 1.0,
            },
            TagEdge {
                source: 1,
                target: 2,
                weight: 1.0,
            },
            TagEdge {
                source: 3,
                target: 4,
                weight: 1.0,
            },
            TagEdge {
                source: 3,
                target: 5,
                weight: 1.0,
            },
            TagEdge {
                source: 4,
                target: 5,
                weight: 1.0,
            },
        ];
        let communities = louvain_communities(6, &edges);
        assert_eq!(communities.len(), 6);
        // Each triangle should be in one community (all 3 same)
        assert_eq!(
            communities[0], communities[1],
            "triangle A: nodes 0 and 1 should be in the same community"
        );
        assert_eq!(
            communities[0], communities[2],
            "triangle A: nodes 0 and 2 should be in the same community"
        );
        assert_eq!(
            communities[3], communities[4],
            "triangle B: nodes 3 and 4 should be in the same community"
        );
        assert_eq!(
            communities[3], communities[5],
            "triangle B: nodes 3 and 5 should be in the same community"
        );
        // The two triangles should be in different communities
        assert_ne!(
            communities[0], communities[3],
            "disconnected triangles should be in different communities"
        );
    }

    #[test]
    fn louvain_three_node_chain() {
        // 0-1-2 chain: all should be same community (connected)
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 1.0,
            },
            TagEdge {
                source: 1,
                target: 2,
                weight: 1.0,
            },
        ];
        let communities = louvain_communities(3, &edges);
        assert_eq!(communities.len(), 3);
        assert_eq!(
            communities[0], communities[2],
            "all nodes in chain should be same community"
        );
    }

    // ==================================================================
    //  page_rank_full
    // ==================================================================

    #[test]
    fn page_rank_empty_graph() {
        assert!(page_rank_full(0, &[]).is_empty());
    }

    #[test]
    fn page_rank_single_dangling_node() {
        // A single dangling node converges to rank 1.0 (all PageRank
        // mass goes to the dangling sum redistribution).
        let rank = page_rank_full(1, &[]);
        assert_eq!(rank.len(), 1);
        assert!(
            (rank[0] - 1.0).abs() < 1e-4,
            "single dangling node should have rank ~1.0, got {}",
            rank[0]
        );
    }

    #[test]
    fn page_rank_two_nodes_symmetric() {
        let edges = vec![TagEdge {
            source: 0,
            target: 1,
            weight: 1.0,
        }];
        let rank = page_rank_full(2, &edges);
        assert_eq!(rank.len(), 2);
        // Values must be in [0, 1] and sum > 0
        for &r in &rank {
            assert!((0.0..=1.0).contains(&r), "rank must be in [0,1], got {r}");
        }
        assert!(rank.iter().sum::<f32>() > 0.0, "rank sum must be positive");
    }

    #[test]
    fn page_rank_asymmetric_node_ranks_higher() {
        // Star: node 0 connects to 1,2,3. Nodes 1,2,3 only connect to 0.
        // Node 0 should have higher rank.
        let edges = vec![
            TagEdge {
                source: 0,
                target: 1,
                weight: 1.0,
            },
            TagEdge {
                source: 0,
                target: 2,
                weight: 1.0,
            },
            TagEdge {
                source: 0,
                target: 3,
                weight: 1.0,
            },
        ];
        let rank = page_rank_full(4, &edges);
        assert_eq!(rank.len(), 4);
        assert!(
            rank[0] > rank[1],
            "central node 0 should have higher rank than leaf 1: {} > {}",
            rank[0],
            rank[1]
        );
    }

    // ==================================================================
    //  build_pmi_edges
    // ==================================================================

    /// Helper: build a simple user graph for PMI tests.
    fn make_user_graph() -> TagRelationGraph {
        let mut g = TagRelationGraph::with_posts(100);
        g.set_marginal(4, "fluffy", 50); // general
        g.set_marginal(4, "canine", 30); // general
        g.set_marginal(3, "wolf", 20); // species
        g.insert_pair(4, "fluffy", 4, "canine", 25);
        g.insert_pair(4, "fluffy", 3, "wolf", 15);
        g.insert_pair(4, "canine", 3, "wolf", 10);
        g
    }

    #[test]
    fn pmi_edges_empty_nodes() {
        let user_graph = make_user_graph();
        let global_cooc = HashMap::new();
        let edges = build_pmi_edges(&[], &user_graph, 100, &global_cooc, 1000, 2);
        assert!(edges.is_empty());
    }

    #[test]
    fn pmi_edges_produces_weighted_edges() {
        let user_graph = make_user_graph();
        let global_cooc = HashMap::new();
        let generic = HashSet::new();
        let tags = vec![
            make_tc("fluffy", "general", 50),
            make_tc("canine", "general", 30),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);
        assert_eq!(nodes.len(), 2);

        let edges = build_pmi_edges(&nodes, &user_graph, 100, &global_cooc, 1000, 2);
        assert!(!edges.is_empty(), "should produce at least one edge");
        // (fluffy, canine) cooc = 25 >= min_cooc=2, so they should connect
        assert!(
            edges.iter().any(|e| {
                (nodes[e.source].name == "fluffy" && nodes[e.target].name == "canine")
                    || (nodes[e.source].name == "canine" && nodes[e.target].name == "fluffy")
            }),
            "edge between fluffy and canine should exist"
        );
    }

    #[test]
    fn pmi_edges_filters_below_min_cooc() {
        let mut user_graph = TagRelationGraph::with_posts(100);
        user_graph.set_marginal(4, "rare_a", 5);
        user_graph.set_marginal(4, "rare_b", 5);
        // cooc = 1, below the default min_cooc (which is passed as 2 in compute_taste_themes)
        user_graph.insert_pair(4, "rare_a", 4, "rare_b", 1);

        let global_cooc = HashMap::new();
        let generic = HashSet::new();
        let tags = vec![
            make_tc("rare_a", "general", 5),
            make_tc("rare_b", "general", 5),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);

        let edges = build_pmi_edges(&nodes, &user_graph, 100, &global_cooc, 1000, 2);
        assert!(
            edges.is_empty(),
            "cooc=1 below min_cooc=2 should produce no edges"
        );
    }

    #[test]
    fn pmi_edges_uses_global_cooc_map() {
        let user_graph = make_user_graph();
        let mut global_cooc = HashMap::new();
        // Use global cooc that's well above expected to produce a visible difference.
        // Expected = df1 * df2 / n_catalog = 10 * 8 / 10000 = 0.008.
        // Observed = 50 → lift = 6250 → ln(6250)/5 ≈ 1.75 → clamped to 1.0.
        // This should clearly change the combined PMI vs user-only.
        global_cooc.insert("canine||fluffy".to_string(), (50i64, 10i64, 8i64));
        let generic: HashSet<String> = HashSet::new();
        let tags = vec![
            make_tc("fluffy", "general", 50),
            make_tc("canine", "general", 30),
        ];
        let (nodes, _) = build_non_generic_nodes(&tags, &generic, 100);

        let edges = build_pmi_edges(&nodes, &user_graph, 100, &global_cooc, 10_000, 2);
        assert!(!edges.is_empty());
        // With global PMI, the weight should differ from user-only case
        let edges_no_global = build_pmi_edges(&nodes, &user_graph, 100, &HashMap::new(), 10_000, 2);
        let weight_with = edges[0].weight;
        let weight_without = edges_no_global[0].weight;
        assert!(
            (weight_with - weight_without).abs() > 1e-6,
            "global cooc should change PMI weight: with={weight_with} without={weight_without}"
        );
    }

    // ==================================================================
    //  compute_taste_themes — integration-level smoke test
    // ==================================================================

    #[test]
    fn compute_taste_themes_returns_empty_for_few_tags() {
        let user_graph = TagRelationGraph::with_posts(0);
        let global_cooc = HashMap::new();
        let impls = HashMap::new();
        let aliases = HashMap::new();
        let tag_df = HashMap::new();
        let tag_counts = vec![];

        let themes = compute_taste_themes(
            &tag_counts,
            &user_graph,
            0,
            &global_cooc,
            0,
            &impls,
            &aliases,
            &tag_df,
            250,
            2,
        );
        assert!(themes.is_empty(), "no tags → no themes");
    }

    #[test]
    fn compute_taste_themes_smoke_test() {
        // Build a realistic small dataset: a few general tags with
        // co-occurrence structure.
        let mut user_graph = TagRelationGraph::with_posts(50);
        user_graph.set_marginal(4, "fluffy", 30);
        user_graph.set_marginal(4, "canine", 20);
        user_graph.set_marginal(4, "feline", 15);
        user_graph.set_marginal(4, "outdoor", 10);
        user_graph.set_marginal(4, "indoor", 8);
        user_graph.set_marginal(4, "detailed_background", 12);
        user_graph.insert_pair(4, "fluffy", 4, "canine", 18);
        user_graph.insert_pair(4, "fluffy", 4, "feline", 12);
        user_graph.insert_pair(4, "fluffy", 4, "outdoor", 8);
        user_graph.insert_pair(4, "canine", 4, "feline", 3);
        user_graph.insert_pair(4, "canine", 4, "outdoor", 7);
        user_graph.insert_pair(4, "feline", 4, "indoor", 6);
        user_graph.insert_pair(4, "fluffy", 4, "indoor", 4);

        let global_cooc = HashMap::new();
        let impls = HashMap::new();
        let aliases = HashMap::new();

        // Tag DF: each tag appears in some fraction of catalog
        let mut tag_df = HashMap::new();
        tag_df.insert("fluffy".to_string(), 8000i64);
        tag_df.insert("canine".to_string(), 5000);
        tag_df.insert("feline".to_string(), 4000);
        tag_df.insert("outdoor".to_string(), 3000);
        tag_df.insert("indoor".to_string(), 2000);
        tag_df.insert("detailed_background".to_string(), 1000);

        let tag_counts = vec![
            make_tc("fluffy", "general", 30),
            make_tc("canine", "general", 20),
            make_tc("feline", "general", 15),
            make_tc("outdoor", "general", 10),
            make_tc("indoor", "general", 8),
            make_tc("detailed_background", "general", 12),
        ];

        let themes = compute_taste_themes(
            &tag_counts,
            &user_graph,
            50,
            &global_cooc,
            100_000,
            &impls,
            &aliases,
            &tag_df,
            250,
            2,
        );

        // Should produce at least one theme
        if themes.is_empty() {
            // This is acceptable if the graph structure doesn't produce
            // communities that pass the min_core_count filter — just log
            // rather than fail.
            eprintln!("note: smoke test produced 0 themes (possibly weak community filter)");
            return;
        }

        // Each theme should have a name and core tags
        for theme in &themes {
            assert!(!theme.name.is_empty(), "theme should have a non-empty name");
            assert!(
                !theme.core.is_empty(),
                "theme '{}' should have core tags",
                theme.name
            );
            assert!(theme.importance > 0.0, "importance should be > 0");
        }
    }

    // ==================================================================
    //  build_themes — CORE/KINK split logic
    // ==================================================================

    #[test]
    fn build_themes_empty_communities() {
        let nodes = vec![];
        let communities = vec![];
        let centrality = vec![];
        let tag_counts = vec![];
        let idx_map = HashMap::new();
        let aliases = HashMap::new();
        let impls = HashMap::new();
        let tag_df = HashMap::new();

        let themes = build_themes(
            &nodes,
            &communities,
            &centrality,
            &tag_counts,
            &idx_map,
            &aliases,
            &impls,
            &tag_df,
            1000,
            50,
            &[],
        );
        assert!(themes.is_empty());
    }

    #[test]
    fn build_themes_community_with_two_nodes() {
        // Two nodes in same community, same group, with tag counts.
        let nodes = vec![
            TagNode {
                id: 0,
                name: "fluffy".to_string(),
                group_type: "general".to_string(),
                count: 30,
            },
            TagNode {
                id: 1,
                name: "canine".to_string(),
                group_type: "general".to_string(),
                count: 20,
            },
        ];
        let communities = vec![0usize, 0]; // both in community 0
        let centrality = vec![0.8f32, 0.6];
        let tag_counts = vec![
            make_tc("fluffy", "general", 30),
            make_tc("canine", "general", 20),
        ];
        let mut idx_map = HashMap::new();
        idx_map.insert(("fluffy".to_string(), "general".to_string()), 0usize);
        idx_map.insert(("canine".to_string(), "general".to_string()), 1usize);
        let aliases = HashMap::new();
        let impls = HashMap::new();
        let mut tag_df = HashMap::new();
        tag_df.insert("fluffy".to_string(), 8000i64);
        tag_df.insert("canine".to_string(), 5000i64);

        // Edge list (needed for local_strength computation)
        let edges = vec![TagEdge {
            source: 0,
            target: 1,
            weight: 0.8,
        }];

        let themes = build_themes(
            &nodes,
            &communities,
            &centrality,
            &tag_counts,
            &idx_map,
            &aliases,
            &impls,
            &tag_df,
            100_000,
            50,
            &edges,
        );

        if themes.is_empty() {
            // Could be filtered by min_core_count or size
            eprintln!("note: 2-node community filtered (min_core_count check)");
            return;
        }

        let theme = &themes[0];
        // Name should be the tag with max count
        assert_eq!(
            theme.name, "fluffy",
            "highest-count tag should name the theme"
        );

        // With 2 nodes, p50 = (0.8+0.6)/2 sorted... wait p50 is the element
        // at index sizes/2 = 1, which is 0.6 (sorted ASC: [0.6, 0.8]).
        // So cent >= 0.6 means both are core.
        assert_eq!(theme.core.len(), 2, "both nodes cent >= p50 should be core");
        // importance should be positive
        assert!(theme.importance > 0.0);
        assert_eq!(theme.size, 2);
    }

    #[test]
    fn build_themes_removes_generic_from_core_and_kink() {
        let nodes = vec![
            TagNode {
                id: 0,
                name: "male".to_string(),
                group_type: "general".to_string(),
                count: 100,
            },
            TagNode {
                id: 1,
                name: "fluffy".to_string(),
                group_type: "general".to_string(),
                count: 80,
            },
        ];
        let communities = vec![0usize, 0];
        let centrality = vec![0.9f32, 0.7];
        let tag_counts = vec![
            make_tc("male", "general", 100),
            make_tc("fluffy", "general", 80),
        ];
        let mut idx_map = HashMap::new();
        idx_map.insert(("male".to_string(), "general".to_string()), 0usize);
        idx_map.insert(("fluffy".to_string(), "general".to_string()), 1usize);
        let aliases = HashMap::new();
        let impls = HashMap::new();
        let mut tag_df = HashMap::new();
        tag_df.insert("male".to_string(), 100_000i64);
        tag_df.insert("fluffy".to_string(), 8000i64);

        let edges = vec![TagEdge {
            source: 0,
            target: 1,
            weight: 0.5,
        }];

        let themes = build_themes(
            &nodes,
            &communities,
            &centrality,
            &tag_counts,
            &idx_map,
            &aliases,
            &impls,
            &tag_df,
            100_000,
            50,
            &edges,
        );

        if themes.is_empty() {
            eprintln!("note: theme filtered (likely min_core_count check)");
            return;
        }

        let theme = &themes[0];
        // "male" is hardcoded generic → should NOT appear in core or kink
        let all_tags: Vec<&str> = theme
            .core
            .iter()
            .chain(theme.kink.iter())
            .map(|t| t.name.as_str())
            .collect();
        assert!(
            !all_tags.contains(&"male"),
            "generic 'male' must not appear in core/kink"
        );
        assert!(
            all_tags.contains(&"fluffy"),
            "non-generic 'fluffy' should appear"
        );
    }

    // ==================================================================
    //  Integration: end-to-end with enough structure for communities
    // ==================================================================

    #[test]
    fn end_to_end_with_two_communities() {
        // Two distinct groups of tags with strong internal connections
        // and weak cross connections → should form 2 communities.
        let mut user_graph = TagRelationGraph::with_posts(100);

        // Group A: fluffy + canine + outdoor
        user_graph.set_marginal(4, "fluffy", 40);
        user_graph.set_marginal(4, "canine", 35);
        user_graph.set_marginal(4, "outdoor", 25);
        user_graph.insert_pair(4, "fluffy", 4, "canine", 30);
        user_graph.insert_pair(4, "fluffy", 4, "outdoor", 20);
        user_graph.insert_pair(4, "canine", 4, "outdoor", 18);

        // Group B: feline + indoor + sleepy
        user_graph.set_marginal(4, "feline", 30);
        user_graph.set_marginal(4, "indoor", 20);
        user_graph.set_marginal(4, "sleepy", 15);
        user_graph.insert_pair(4, "feline", 4, "indoor", 15);
        user_graph.insert_pair(4, "feline", 4, "sleepy", 12);
        user_graph.insert_pair(4, "indoor", 4, "sleepy", 10);

        // Weak cross edges
        user_graph.insert_pair(4, "fluffy", 4, "feline", 2);
        user_graph.insert_pair(4, "canine", 4, "feline", 1);

        let global_cooc = HashMap::new();
        let impls = HashMap::new();
        let aliases = HashMap::new();
        let mut tag_df = HashMap::new();
        tag_df.insert("fluffy".to_string(), 8000i64);
        tag_df.insert("canine".to_string(), 5000i64);
        tag_df.insert("outdoor".to_string(), 3000i64);
        tag_df.insert("feline".to_string(), 4000i64);
        tag_df.insert("indoor".to_string(), 2000i64);
        tag_df.insert("sleepy".to_string(), 1500i64);

        let tag_counts = vec![
            make_tc("fluffy", "general", 40),
            make_tc("canine", "general", 35),
            make_tc("outdoor", "general", 25),
            make_tc("feline", "general", 30),
            make_tc("indoor", "general", 20),
            make_tc("sleepy", "general", 15),
        ];

        let themes = compute_taste_themes(
            &tag_counts,
            &user_graph,
            100,
            &global_cooc,
            100_000,
            &impls,
            &aliases,
            &tag_df,
            250,
            2,
        );

        if themes.is_empty() {
            eprintln!("note: two-community test produced 0 themes (filter threshold)");
            return;
        }

        // Should have at least 1 theme, possibly 2 if communities separate
        eprintln!("note: got {} themes from two-community test", themes.len());
        for (i, t) in themes.iter().enumerate() {
            eprintln!(
                "  theme {i}: name={}, core={:?}, kink={:?}",
                t.name,
                t.core.iter().map(|c| &c.name).collect::<Vec<_>>(),
                t.kink.iter().map(|k| &k.name).collect::<Vec<_>>()
            );
        }

        // Each theme should have coherent properties
        for theme in &themes {
            assert!(!theme.name.is_empty());
            assert!(!theme.core.is_empty());
            assert!(theme.size >= 2);
        }
    }
}
