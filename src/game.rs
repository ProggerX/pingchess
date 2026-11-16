//! Chess rules and game state, backed by `shakmaty`.

use shakmaty::fen::Fen;
use shakmaty::san::SanPlus;
use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Chess, Color, EnPassantMode, Move, Position};

#[derive(Debug, Clone)]
pub struct Entry {
    pub san: SanPlus,
}

#[derive(Debug, Clone)]
pub struct Applied {
    pub uci: UciMove,
    pub san: SanPlus,
    /// Half-move index before the move was played.
    pub ply: u16,
    /// Position hash after the move.
    pub hash: u64,
    pub result: Option<GameResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameResult {
    Checkmate { winner: Color },
    Resignation { winner: Color },
    Stalemate,
    InsufficientMaterial,
    FiftyMove,
    ThreefoldRepetition,
}

impl GameResult {
    pub fn winner(self) -> Option<Color> {
        match self {
            GameResult::Checkmate { winner } | GameResult::Resignation { winner } => Some(winner),
            GameResult::Stalemate
            | GameResult::InsufficientMaterial
            | GameResult::FiftyMove
            | GameResult::ThreefoldRepetition => None,
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            GameResult::Checkmate { .. } => "checkmate",
            GameResult::Resignation { .. } => "resignation",
            GameResult::Stalemate => "stalemate",
            GameResult::InsufficientMaterial => "insufficient material",
            GameResult::FiftyMove => "the fifty-move rule",
            GameResult::ThreefoldRepetition => "threefold repetition",
        }
    }

    pub fn pgn(self) -> &'static str {
        match self.winner() {
            Some(Color::White) => "1-0",
            Some(Color::Black) => "0-1",
            None => "1/2-1/2",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Game {
    pos: Chess,
    /// Every position that has occurred, including the initial position.
    history: Vec<Chess>,
    entries: Vec<Entry>,
    last_move: Option<Move>,
    result: Option<GameResult>,
}

impl Game {
    pub fn new() -> Game {
        let pos = Chess::default();
        Game {
            pos: pos.clone(),
            history: vec![pos],
            entries: Vec::new(),
            last_move: None,
            result: None,
        }
    }

    pub fn position(&self) -> &Chess {
        &self.pos
    }

    pub fn turn(&self) -> Color {
        self.pos.turn()
    }

    pub fn last_move(&self) -> Option<Move> {
        self.last_move
    }

    pub fn result(&self) -> Option<GameResult> {
        self.result
    }

    pub fn is_over(&self) -> bool {
        self.result.is_some()
    }

    /// Number of half-moves played so far.
    pub fn ply(&self) -> u16 {
        self.entries.len() as u16
    }

    pub fn fen(&self) -> String {
        Fen::from_position(&self.pos, EnPassantMode::Legal).to_string()
    }

    /// Parses and plays a move in UCI notation (the wire format).
    pub fn apply_uci(&mut self, text: &str) -> Result<Applied, String> {
        let uci = UciMove::from_ascii(text.as_bytes())
            .map_err(|_| format!("'{text}' is not valid UCI notation"))?;
        let mv = uci
            .to_move(&self.pos)
            .map_err(|_| format!("{text} is not a legal move in this position"))?;
        self.apply_move(mv)
    }

    /// Parses and plays either UCI or SAN notation (what a human types).
    pub fn apply_input(&mut self, text: &str) -> Result<Applied, String> {
        if let Ok(uci) = UciMove::from_ascii(text.as_bytes())
            && let Ok(mv) = uci.to_move(&self.pos)
        {
            return self.apply_move(mv);
        }

        // Accept 0-0 and 0-0-0 as well as O-O / O-O-O.
        let normalized = text.replace('0', "O");
        let san = SanPlus::from_ascii(normalized.as_bytes())
            .map_err(|_| format!("'{text}' is not a legal UCI or SAN move"))?;
        let mv = san
            .san
            .to_move(&self.pos)
            .map_err(|_| format!("{text} is not legal in this position"))?;
        self.apply_move(mv)
    }

    pub fn apply_move(&mut self, mv: Move) -> Result<Applied, String> {
        if let Some(result) = self.result {
            return Err(format!("the game is already over ({})", result.reason()));
        }
        if !self.pos.is_legal(mv) {
            return Err("illegal move".into());
        }

        let mover = self.pos.turn();
        let ply = self.entries.len() as u16;
        let uci = UciMove::from_move(mv, CastlingMode::Standard);
        let san = SanPlus::from_move(self.pos.clone(), mv);

        self.pos = self
            .pos
            .clone()
            .play(mv)
            .map_err(|_| "illegal move".to_string())?;
        self.history.push(self.pos.clone());
        self.last_move = Some(mv);
        self.entries.push(Entry { san });

        let result = self.detect_result(mover);
        self.result = result;

        Ok(Applied {
            uci,
            san,
            ply,
            hash: position_hash(&self.pos),
            result,
        })
    }

    pub fn resign(&mut self, color: Color) {
        if self.result.is_none() {
            self.result = Some(GameResult::Resignation {
                winner: color.other(),
            });
        }
    }

    pub fn legal_moves_san(&self) -> Vec<(String, String)> {
        self.pos
            .legal_moves()
            .iter()
            .map(|mv| {
                let san = SanPlus::from_move(self.pos.clone(), *mv).to_string();
                let uci = UciMove::from_move(*mv, CastlingMode::Standard).to_string();
                (san, uci)
            })
            .collect()
    }

    pub fn movetext(&self) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < self.entries.len() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&format!("{}. {}", i / 2 + 1, self.entries[i].san));
            if i + 1 < self.entries.len() {
                out.push(' ');
                out.push_str(&self.entries[i + 1].san.to_string());
                i += 2;
            } else {
                i += 1;
            }
        }
        out
    }

    fn detect_result(&self, mover: Color) -> Option<GameResult> {
        if self.pos.is_checkmate() {
            Some(GameResult::Checkmate { winner: mover })
        } else if self.pos.is_stalemate() {
            Some(GameResult::Stalemate)
        } else if self.pos.is_insufficient_material() {
            Some(GameResult::InsufficientMaterial)
        } else if self.pos.halfmoves() >= 100 {
            Some(GameResult::FiftyMove)
        } else if self.repetition_count() >= 3 {
            Some(GameResult::ThreefoldRepetition)
        } else {
            None
        }
    }

    fn repetition_count(&self) -> usize {
        self.history
            .iter()
            .filter(|position| **position == self.pos)
            .count()
    }
}

/// Stable fingerprint of a position, used to detect desynchronisation.
pub fn position_hash(pos: &Chess) -> u64 {
    let fen = Fen::from_position(pos, EnPassantMode::Always).to_string();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in fen.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_from_the_initial_position() {
        let game = Game::new();
        assert_eq!(game.ply(), 0);
        assert_eq!(game.turn(), Color::White);
        assert!(game.result().is_none());
        assert_eq!(
            game.fen(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"
        );
    }

    #[test]
    fn fools_mate() {
        let mut game = Game::new();
        let moves = ["f2f3", "e7e5", "g2g4", "d8h4"];
        let mut last = None;
        for text in moves {
            last = Some(game.apply_uci(text).unwrap());
        }
        let last = last.unwrap();
        assert_eq!(last.san.to_string(), "Qh4#");
        assert_eq!(
            game.result(),
            Some(GameResult::Checkmate {
                winner: Color::Black
            })
        );
        assert_eq!(game.result().unwrap().pgn(), "0-1");
    }

    #[test]
    fn parses_san_and_castling_notation() {
        let mut game = Game::new();
        assert_eq!(game.apply_input("e4").unwrap().san.to_string(), "e4");
        assert_eq!(game.apply_input("e5").unwrap().san.to_string(), "e5");
        assert_eq!(game.apply_input("Nf3").unwrap().san.to_string(), "Nf3");
        assert_eq!(game.apply_input("Nc6").unwrap().san.to_string(), "Nc6");
        assert_eq!(game.apply_input("Bc4").unwrap().san.to_string(), "Bc4");
        assert_eq!(game.apply_input("Bc5").unwrap().san.to_string(), "Bc5");
        assert_eq!(game.apply_input("0-0").unwrap().san.to_string(), "O-O");
    }

    #[test]
    fn rejects_illegal_moves() {
        let mut game = Game::new();
        assert!(game.apply_uci("e2e5").is_err());
        assert!(game.apply_uci("not a move").is_err());
        assert_eq!(game.ply(), 0);
    }

    #[test]
    fn detects_threefold_repetition() {
        let mut game = Game::new();
        for _ in 0..2 {
            for mv in ["g1f3", "g8f6", "f3g1", "f6g8"] {
                game.apply_uci(mv).unwrap();
            }
        }
        assert_eq!(game.result(), Some(GameResult::ThreefoldRepetition));
        assert_eq!(game.result().unwrap().pgn(), "1/2-1/2");
    }

    #[test]
    fn hashes_positions_deterministically() {
        let game = Game::new();
        let a = position_hash(game.position());
        let b = position_hash(game.position());
        assert_eq!(a, b);

        let mut game = Game::new();
        game.apply_uci("e2e4").unwrap();
        assert_ne!(a, position_hash(game.position()));
    }
}
