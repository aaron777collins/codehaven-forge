//! Generated SVG avatars: a seeded identicon, a rounded coloured square with a
//! horizontally symmetric 5x5 grid of white cells. Pure functions, no IO.

use sha2::{Digest, Sha256};

/// Background colours, hand picked so near white cells at 0.92 opacity keep a
/// contrast of at least 3:1 on every one of them.
const BACKGROUNDS: [&str; 10] =
    ["#0f766e", "#1d4ed8", "#7c3aed", "#be185d", "#b45309", "#15803d", "#4338ca", "#0e7490", "#a21caf", "#b91c1c"];

/// Independent cells in the grid: columns 0, 1 and 2, one bit per row. Columns 4
/// and 3 mirror 0 and 1, column 2 sits on the axis of symmetry.
const CELLS: usize = 15;

/// The grid must show at least this many filled cells.
const MIN_FILLED: usize = 6;

/// Renders a deterministic avatar for `seed`. Same seed, same picture, derived
/// from `sha256(seed)`: background colour from the first digest byte, the grid
/// from the digest bits, topped up to at least six filled cells. Every element
/// carries attributes only, never `style`, so the output is safe to inline
/// under a strict CSP.
pub fn svg(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let hash = hex::encode(digest);

    let background = BACKGROUNDS[digest[0] as usize % BACKGROUNDS.len()];

    // Fifteen grid bits straight off the first four hex digits.
    let mut cells = [false; CELLS];
    for (i, cell) in cells.iter_mut().enumerate() {
        *cell = bit(&hash, i);
    }

    // Fewer than six filled: flip hash-chosen cells on until the grid is busy
    // enough. If the digest somehow runs out first, fill empty cells in order.
    let mut k = 4; // hex digits 0 to 3 chose the grid, flip from digit 4 on
    while filled(&cells) < MIN_FILLED && k < hash.len() {
        cells[nibble(&hash, k) as usize % CELLS] = true;
        k += 1;
    }
    let mut count = filled(&cells);
    for cell in cells.iter_mut() {
        if count >= MIN_FILLED {
            break;
        }
        if !*cell {
            *cell = true;
            count += 1;
        }
    }

    let mut out = String::with_capacity(512);
    out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 80 80\" role=\"img\" aria-hidden=\"true\">");
    out.push_str("<rect x=\"0\" y=\"0\" width=\"80\" height=\"80\" rx=\"16\" fill=\"");
    out.push_str(background);
    out.push_str("\"/>");
    for row in 0..5usize {
        for col in 0..5usize {
            let i = match col {
                0 | 4 => row * 3,
                1 | 3 => row * 3 + 1,
                _ => row * 3 + 2,
            };
            if !cells[i] {
                continue;
            }
            out.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"10\" fill=\"#ffffff\" opacity=\"0.92\"/>",
                15 + col * 10,
                15 + row * 10
            ));
        }
    }
    out.push_str("</svg>");
    out
}

/// The value of the `i`-th hex digit of `hash`.
fn nibble(hash: &str, i: usize) -> u32 {
    (hash.as_bytes()[i] as char).to_digit(16).expect("hex digit from hex::encode")
}

/// The `i`-th bit of `hash`, most significant bit of each digit first.
fn bit(hash: &str, i: usize) -> bool {
    (nibble(hash, i / 4) >> (3 - i % 4)) & 1 == 1
}

/// How many grid cells are filled.
fn filled(cells: &[bool; CELLS]) -> usize {
    cells.iter().filter(|cell| **cell).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_output() {
        assert_eq!(svg("alice"), svg("alice"));
    }

    #[test]
    fn different_seeds_differ() {
        assert_ne!(svg("alice"), svg("bob"));
        assert_ne!(svg("bob"), svg("carol"));
    }

    #[test]
    fn valid_svg_start_and_end() {
        let out = svg("alice");
        assert!(out.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 80 80\" role=\"img\" aria-hidden=\"true\">"));
        assert!(out.ends_with("</svg>"));
    }

    #[test]
    fn no_inline_style() {
        for seed in ["alice", "bob", "", "a seed with spaces & \"quotes\""] {
            assert!(!svg(seed).contains("style="));
        }
    }

    #[test]
    fn at_least_six_cells_plus_background() {
        for seed in ["alice", "bob", "carol", "dave", "eve"] {
            let out = svg(seed);
            let rects = out.matches("<rect ").count();
            assert!(rects >= 7, "seed {seed} drew only {rects} rects");
            let cells = out.matches("opacity=\"0.92\"").count();
            assert!(cells >= 6, "seed {seed} filled only {cells} cells");
            assert!(out.contains("rx=\"16\""), "seed {seed} has no rounded background");
        }
    }
}
