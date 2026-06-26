//! Pure screen geometry: edge-band tests and the exit→entry point mapping.
//!
//! Kept free of any I/O so the crossing math is exhaustively unit-testable
//! without an X server. Pixel coordinates are inclusive `[0, dim-1]`, matching
//! what the OS clamps the real cursor against.

use mouse_core::config::Edge;

/// Is `(x, y)` within `threshold` pixels of `edge`, on a `w`×`h` screen?
///
/// Used for both the server's exit edge and the client's entry edge.
pub fn in_edge_band(edge: Edge, x: i32, y: i32, w: i32, h: i32, threshold: i32) -> bool {
    let t = threshold.max(0);
    match edge {
        Edge::Left => x <= t,
        Edge::Right => x >= w - 1 - t,
        Edge::Top => y <= t,
        Edge::Bottom => y >= h - 1 - t,
    }
}

/// The free axis of `edge` is the one the cursor slides along *on* that edge:
/// vertical edges (Left/Right) vary in Y, horizontal edges (Top/Bottom) in X.
fn free_axis_is_y(edge: Edge) -> bool {
    matches!(edge, Edge::Left | Edge::Right)
}

/// Map the cursor's exit position to the entry point on the peer's screen.
///
/// The fraction of the way along the exit edge's free axis is preserved onto the
/// entry edge's free axis, and the entry's fixed coordinate is pinned to the edge.
/// This is exact for opposing edge pairs (Left↔Right, Top↔Bottom) — the common
/// side-by-side layout — and stays on-screen for any other pairing.
///
/// * `exit` — the cursor position on the local (server) screen at crossing.
/// * `local` — local screen size `(w, h)`.
/// * `peer` — the peer (client) screen size `(w, h)`.
pub fn entry_point(
    exit_edge: Edge,
    entry_edge: Edge,
    exit: (i32, i32),
    local: (i32, i32),
    peer: (i32, i32),
) -> (i32, i32) {
    let (lw, lh) = local;
    let (pw, ph) = peer;

    // Fraction along the exit edge's free axis, in [0, 1].
    let (free, free_dim) = if free_axis_is_y(exit_edge) {
        (exit.1, lh)
    } else {
        (exit.0, lw)
    };
    let f = if free_dim > 1 {
        (free as f64 / (free_dim - 1) as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Project that fraction onto the entry edge's free axis.
    let entry_free_dim = if free_axis_is_y(entry_edge) { ph } else { pw };
    let efc = (f * (entry_free_dim - 1).max(0) as f64).round() as i32;

    let (x, y) = match entry_edge {
        Edge::Left => (0, efc),
        Edge::Right => (pw - 1, efc),
        Edge::Top => (efc, 0),
        Edge::Bottom => (efc, ph - 1),
    };
    (x.clamp(0, (pw - 1).max(0)), y.clamp(0, (ph - 1).max(0)))
}

/// Opposing edge pair (Left↔Right or Top↔Bottom)? The MVP entry mapping is exact
/// only for these; mixed pairs still stay on-screen but the placement is loose.
pub fn is_opposing(exit_edge: Edge, entry_edge: Edge) -> bool {
    matches!(
        (exit_edge, entry_edge),
        (Edge::Left, Edge::Right)
            | (Edge::Right, Edge::Left)
            | (Edge::Top, Edge::Bottom)
            | (Edge::Bottom, Edge::Top)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_band_detects_each_edge() {
        let (w, h) = (1920, 1080);
        assert!(in_edge_band(Edge::Left, 1, 500, w, h, 2));
        assert!(!in_edge_band(Edge::Left, 5, 500, w, h, 2));
        assert!(in_edge_band(Edge::Right, 1918, 500, w, h, 2)); // 1920-1-2 = 1917
        assert!(!in_edge_band(Edge::Right, 1916, 500, w, h, 2));
        assert!(in_edge_band(Edge::Top, 500, 0, w, h, 2));
        assert!(in_edge_band(Edge::Bottom, 500, 1079, w, h, 2));
        assert!(!in_edge_band(Edge::Bottom, 500, 1076, w, h, 2));
    }

    #[test]
    fn left_exit_to_right_entry_preserves_vertical_fraction() {
        // Server 2560x1440, client 1920x1080. Exit at the left edge, y at the
        // exact middle -> enter at the client's right edge, y at its middle.
        let local = (2560, 1440);
        let peer = (1920, 1080);
        let (x, y) = entry_point(Edge::Left, Edge::Right, (0, 720), local, peer);
        assert_eq!(x, 1919); // right edge of client
        // f = 720/1439 ≈ 0.5003 -> round(0.5003 * 1079) = 540
        assert_eq!(y, 540);
    }

    #[test]
    fn right_exit_to_left_entry_top_and_bottom_corners() {
        let local = (1920, 1080);
        let peer = (2560, 1440);
        // Top corner: y=0 -> y=0 at the left edge.
        assert_eq!(entry_point(Edge::Right, Edge::Left, (1919, 0), local, peer), (0, 0));
        // Bottom corner: y=1079 -> f=1 -> y=1439 at the left edge.
        assert_eq!(
            entry_point(Edge::Right, Edge::Left, (1919, 1079), local, peer),
            (0, 1439)
        );
    }

    #[test]
    fn top_bottom_pair_preserves_horizontal_fraction() {
        let local = (1000, 800);
        let peer = (500, 400);
        // Exit top at x=999 (far right) -> enter bottom at x=499, y=399.
        let (x, y) = entry_point(Edge::Top, Edge::Bottom, (999, 0), local, peer);
        assert_eq!((x, y), (499, 399));
    }

    #[test]
    fn entry_point_always_on_screen() {
        let local = (2560, 1440);
        let peer = (1920, 1080);
        for &exit in &[Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            for &entry in &[Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                for &(px, py) in &[(0, 0), (2559, 0), (0, 1439), (2559, 1439), (1280, 720)] {
                    let (x, y) = entry_point(exit, entry, (px, py), local, peer);
                    assert!((0..peer.0).contains(&x), "x={x} off-screen for {exit:?}->{entry:?}");
                    assert!((0..peer.1).contains(&y), "y={y} off-screen for {exit:?}->{entry:?}");
                }
            }
        }
    }

    #[test]
    fn opposing_pairs() {
        assert!(is_opposing(Edge::Left, Edge::Right));
        assert!(is_opposing(Edge::Bottom, Edge::Top));
        assert!(!is_opposing(Edge::Left, Edge::Top));
        assert!(!is_opposing(Edge::Left, Edge::Left));
    }
}
