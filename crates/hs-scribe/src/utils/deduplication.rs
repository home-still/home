use crate::models::layout::BBox;

/// What fraction of `child`'s area is inside `parent`?
pub(crate) fn containment_ratio(parent: &BBox, child: &BBox) -> f32 {
    let x1 = parent.x1.max(child.x1);
    let y1 = parent.y1.max(child.y1);
    let x2 = parent.x2.min(child.x2);
    let y2 = parent.y2.min(child.y2);
    if x2 <= x1 || y2 <= y1 {
        return 0.0;
    }
    let intersection = (x2 - x1) * (y2 - y1);
    let child_area = child.area();
    if child_area > 0.0 {
        intersection / child_area
    } else {
        0.0
    }
}

/// Filter hierarchical containment artifacts from PP-DocLayout-V3.
///
/// PP-DocLayout-V3 outputs parent containers alongside child entries.
/// This handles specific known patterns where the parent is redundant:
///
/// 1. `reference` containing ≥2 `reference_content` → drop parent (children are precise)
/// 2. `table` containing sub-`table`s → drop children (parent is the full table for SLANet)
/// 3. `text`/`inline_formula` → keep both (different scoring metrics)
pub fn filter_contained_regions(boxes: Vec<BBox>) -> Vec<BBox> {
    if boxes.len() <= 1 {
        return boxes;
    }

    let mut drop_indices: Vec<bool> = vec![false; boxes.len()];

    for (i, parent) in boxes.iter().enumerate() {
        if drop_indices[i] {
            continue;
        }

        // Rule 1: reference containing reference_content → drop parent
        if parent.class_name == "reference" {
            let contained_children: usize = boxes
                .iter()
                .enumerate()
                .filter(|&(j, child)| {
                    j != i
                        && !drop_indices[j]
                        && child.class_name == "reference_content"
                        && containment_ratio(parent, child) > 0.80
                })
                .count();

            if contained_children >= 2 {
                drop_indices[i] = true;
                continue;
            }
        }

        // Note: table-in-table containment is left as-is. SLANet processes
        // each table bbox independently; sub-tables are handled by the table
        // structure recognizer and dropping either direction hurts TEDS.
    }

    boxes
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop_indices[*i])
        .map(|(_, b)| b)
        .collect()
}

/// Deduplicate bounding boxes using class-aware NMS
///
/// Removes duplicate/overlapping boxes:
/// 1. Exact duplicates (IoU >= 0.95): Always drop lower-confidence
/// 2. Significant cross-class overlaps (IoU >= 0.7): Keep higher priority;
///    at same priority, keep higher confidence (already sorted descending)
pub fn deduplicate_boxes(mut boxes: Vec<BBox>) -> Vec<BBox> {
    boxes.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut result = Vec::new();

    for candidate in boxes {
        let mut should_keep = true;

        for kept in &result {
            let iou = candidate.iou(kept);

            // Exact duplicates (IoU >= 0.95): always drop lower-confidence
            if iou >= 0.95 {
                should_keep = false;
                break;
            }

            // Cross-class overlaps (IoU >= 0.7): drop lower priority,
            // or same priority with lower confidence
            if iou >= 0.7 && candidate.class_name != kept.class_name {
                let cand_pri = class_priority(&candidate.class_name);
                let kept_pri = class_priority(&kept.class_name);
                if cand_pri >= kept_pri {
                    should_keep = false;
                    break;
                }
            }
        }

        if should_keep {
            result.push(candidate);
        }
    }

    result
}

/// Get priority for class name (lower number = higher priority)
fn class_priority(class: &str) -> u8 {
    match class {
        "doc_title" | "paragraph_title" | "figure_title" => 1,
        "image" | "chart" | "table" | "seal" => 2,
        "display_formula" | "inline_formula" => 3,
        "text" | "abstract" | "content" | "reference" | "reference_content" | "footnote"
        | "vision_footnote" | "aside_text" | "vertical_text" | "algorithm" => 4,
        // Legacy YOLO names
        "title" | "caption" | "section_header" => 1,
        "figure" | "equation" => 2,
        "plain text" | "paragraph" => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bbox(x1: f32, y1: f32, x2: f32, y2: f32, class: &str, conf: f32, uid: usize) -> BBox {
        BBox {
            x1,
            y1,
            x2,
            y2,
            confidence: conf,
            class_id: 0,
            class_name: class.to_string(),
            unique_id: uid,
            read_order: 0.0,
        }
    }

    #[test]
    fn test_deduplicate_exact_duplicates() {
        let boxes = vec![
            make_bbox(0.0, 0.0, 10.0, 10.0, "text", 0.9, 0),
            make_bbox(0.0, 0.0, 10.0, 10.0, "text", 0.7, 1), // exact dup, lower conf
        ];
        let result = deduplicate_boxes(boxes);
        assert_eq!(result.len(), 1);
        assert!((result[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_deduplicate_cross_class_overlap() {
        // IoU ~0.82 (above 0.7, below 0.95) — cross-class rule applies.
        // text (conf 0.9) enters first; table candidate has higher priority (2<4) → kept.
        let boxes = vec![
            make_bbox(0.0, 0.0, 10.0, 10.0, "text", 0.9, 0),
            make_bbox(0.5, 0.5, 10.5, 10.5, "table", 0.8, 1),
        ];
        let result = deduplicate_boxes(boxes);
        assert_eq!(result.len(), 2); // both kept: table has higher priority

        // Lower-priority candidate IS dropped (text pri=4 >= table pri=2)
        let boxes2 = vec![
            make_bbox(0.0, 0.0, 10.0, 10.0, "table", 0.9, 0),
            make_bbox(0.5, 0.5, 10.5, 10.5, "text", 0.8, 1),
        ];
        let result2 = deduplicate_boxes(boxes2);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0].class_name, "table");
    }

    #[test]
    fn test_deduplicate_no_overlap() {
        let boxes = vec![
            make_bbox(0.0, 0.0, 10.0, 10.0, "text", 0.9, 0),
            make_bbox(50.0, 50.0, 60.0, 60.0, "text", 0.8, 1),
        ];
        let result = deduplicate_boxes(boxes);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_contained_reference() {
        // reference containing ≥2 reference_content → drop parent
        let parent = make_bbox(0.0, 0.0, 100.0, 100.0, "reference", 0.9, 0);
        let child1 = make_bbox(5.0, 5.0, 50.0, 50.0, "reference_content", 0.8, 1);
        let child2 = make_bbox(5.0, 55.0, 50.0, 95.0, "reference_content", 0.8, 2);
        let result = filter_contained_regions(vec![parent, child1, child2]);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|b| b.class_name == "reference_content"));
    }

    #[test]
    fn test_filter_contained_single_child() {
        // reference with only 1 reference_content child → keep parent
        let parent = make_bbox(0.0, 0.0, 100.0, 100.0, "reference", 0.9, 0);
        let child = make_bbox(5.0, 5.0, 50.0, 50.0, "reference_content", 0.8, 1);
        let result = filter_contained_regions(vec![parent, child]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_containment_ratio() {
        let parent = make_bbox(0.0, 0.0, 100.0, 100.0, "reference", 0.9, 0);
        let fully_inside = make_bbox(10.0, 10.0, 50.0, 50.0, "text", 0.8, 1);
        assert!((containment_ratio(&parent, &fully_inside) - 1.0).abs() < f32::EPSILON);

        let half_outside = make_bbox(50.0, 0.0, 150.0, 100.0, "text", 0.8, 2);
        assert!((containment_ratio(&parent, &half_outside) - 0.5).abs() < f32::EPSILON);

        let no_overlap = make_bbox(200.0, 200.0, 300.0, 300.0, "text", 0.8, 3);
        assert!((containment_ratio(&parent, &no_overlap) - 0.0).abs() < f32::EPSILON);
    }
}
