//! Terminal rendering.

use shakmaty::{Chess, Color, File, Move, Position, Rank, Square};

use crate::game::GameResult;

const RESET: &str = "\x1b[0m";
const LIGHT_BG: u8 = 238;
const DARK_BG: u8 = 235;
const LAST_MOVE_BG: u8 = 58;
const CHECK_BG: u8 = 88;
const WHITE_PIECE: u8 = 255;
const BLACK_PIECE: u8 = 45;

pub const HELP: &str = "\
Commands:
  board, b       show the board
  moves, legal   list legal moves in SAN and UCI
  history, log   show the move list
  fen            show the current position as FEN
  pgn            show the game as PGN
  flip           flip the board
  resign         resign the game
  trace          toggle hex dumps of ICMP packets
  quit, exit     resign (if needed) and quit
  help, ?        show this help

Moves may be typed as UCI (e2e4, e7e8q) or SAN (e4, Nf3, O-O).";

pub struct BoardStyle {
    pub unicode: bool,
    pub color: bool,
    pub orientation: Color,
}

pub fn render_board(pos: &Chess, last_move: Option<Move>, style: &BoardStyle) -> String {
    let white_at_bottom = style.orientation == Color::White;
    let files: Vec<File> = if white_at_bottom {
        (0..8).map(File::new).collect()
    } else {
        (0..8).rev().map(File::new).collect()
    };
    let ranks: Vec<Rank> = if white_at_bottom {
        (0..8).rev().map(Rank::new).collect()
    } else {
        (0..8).map(Rank::new).collect()
    };

    let last_squares = last_move.map(move_squares);
    let checked_king = if pos.is_check() {
        pos.board().king_of(pos.turn())
    } else {
        None
    };

    let mut out = String::new();

    out.push_str("   ");
    for file in &files {
        out.push(' ');
        out.push(file.char());
        out.push(' ');
    }
    out.push('\n');

    for rank in &ranks {
        out.push(' ');
        out.push(rank.char());
        out.push(' ');

        for file in &files {
            let square = Square::from_coords(*file, *rank);
            let piece = pos.board().piece_at(square);
            let glyph = piece.map_or(' ', |piece| {
                if style.unicode {
                    piece.unicode_char()
                } else {
                    piece.char()
                }
            });

            if style.color {
                let background = if checked_king == Some(square) {
                    CHECK_BG
                } else if last_squares.is_some_and(|(from, to)| from == square || to == square) {
                    LAST_MOVE_BG
                } else if square.is_light() {
                    LIGHT_BG
                } else {
                    DARK_BG
                };
                let foreground = match piece {
                    Some(piece) if piece.color == Color::White => WHITE_PIECE,
                    _ => BLACK_PIECE,
                };
                out.push_str(&format!(
                    "\x1b[1;48;5;{background};38;5;{foreground}m {glyph} {RESET}"
                ));
            } else {
                out.push(' ');
                out.push(glyph);
                out.push(' ');
            }
        }

        out.push(' ');
        out.push(rank.char());
        out.push('\n');
    }

    out.push_str("   ");
    for file in &files {
        out.push(' ');
        out.push(file.char());
        out.push(' ');
    }
    out.push('\n');

    out
}

fn move_squares(mv: Move) -> (Square, Square) {
    match mv {
        Move::Normal { from, to, .. } | Move::EnPassant { from, to, .. } => (from, to),
        Move::Castle { king, rook } => (king, rook),
        Move::Put { to, .. } => (to, to),
    }
}

/// Replaces control characters so a remote player's name cannot inject
/// terminal escape sequences into our output.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

pub fn color_name(color: Color) -> &'static str {
    match color {
        Color::White => "White",
        Color::Black => "Black",
    }
}

pub fn prompt(color: Color) -> String {
    format!("\n{} to move. Your move (UCI or SAN): ", color_name(color))
}

pub fn result_line(result: GameResult, our_color: Color, peer_name: &str) -> String {
    match result {
        GameResult::Resignation { winner } => {
            if winner == our_color {
                format!("{peer_name} resigned. You win!")
            } else {
                format!("You resigned. {peer_name} wins.")
            }
        }
        _ => match result.winner() {
            Some(winner) if winner == our_color => {
                format!("{}! You win!", capitalize(result.reason()))
            }
            Some(_) => format!("{}! {peer_name} wins.", capitalize(result.reason())),
            None => format!("Draw by {}.", result.reason()),
        },
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_starting_position() {
        let pos = Chess::default();
        let style = BoardStyle {
            unicode: false,
            color: false,
            orientation: Color::White,
        };
        let board = render_board(&pos, None, &style);
        let rank_8: Vec<&str> = board
            .lines()
            .find(|line| line.trim_start().starts_with('8'))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(&rank_8[..9], &["8", "r", "n", "b", "q", "k", "b", "n", "r"]);
        let rank_1: Vec<&str> = board
            .lines()
            .find(|line| line.trim_start().starts_with('1'))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(&rank_1[..9], &["1", "R", "N", "B", "Q", "K", "B", "N", "R"]);
        assert_eq!(board.matches('p').count(), 8);
        assert_eq!(board.matches('P').count(), 8);
    }

    #[test]
    fn can_flip_the_board() {
        let pos = Chess::default();
        let style = BoardStyle {
            unicode: false,
            color: false,
            orientation: Color::Black,
        };
        let board = render_board(&pos, None, &style);
        let first_rank: Vec<&str> = board.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(
            &first_rank[..9],
            &["1", "R", "N", "B", "K", "Q", "B", "N", "R"]
        );
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(sanitize("alice\n\x1b[31m"), "alice??[31m");
    }

    #[test]
    fn formats_results_from_both_sides() {
        let checkmate = GameResult::Checkmate {
            winner: Color::White,
        };
        assert_eq!(
            result_line(checkmate, Color::White, "bob"),
            "Checkmate! You win!"
        );
        assert_eq!(
            result_line(checkmate, Color::Black, "alice"),
            "Checkmate! alice wins."
        );
        assert_eq!(
            result_line(GameResult::Stalemate, Color::White, "bob"),
            "Draw by stalemate."
        );
    }
}
