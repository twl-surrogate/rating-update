use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use fxhash::FxHashMap;
use glicko2::{GameResult, Glicko2Rating};
use glob::glob;
use lazy_static::lazy_static;
use rocket::serde::json::serde_json;
use rusqlite::{params, Connection, Row, Transaction};
use serde::Deserialize;
use std::{error::Error, fs::File, io::BufReader, sync::Mutex, time::Duration};
use tokio::{time, try_join};

use crate::website;

const SYS_CONSTANT: f64 = 0.1;
pub const MAX_DEVIATION: f64 = 100.0 / 173.7178;
pub const HIGH_RATING: f64 = (1800.0 - 1500.0) / 173.7178;
const DB_NAME: &str = "ratings.sqlite";

pub const RATING_PERIOD: i64 = 1 * 60 * 60;

pub fn glicko_to_glicko2(r: f64) -> f64 {
    (r - 1500.0) / 173.7178
}

lazy_static! {
    pub static ref RUNTIME_DATA: Mutex<RuntimeData> = Mutex::new(RuntimeData {});
}

pub struct RuntimeData {}

pub fn init_database() -> Result<(), Box<dyn Error>> {
    info!("Intializing database");

    let conn = Connection::open(DB_NAME)?;
    conn.execute_batch(include_str!("../init.sql"))?;

    Ok(())
}

pub fn reset_database() -> Result<(), Box<dyn Error>> {
    info!("Resettting database");
    let conn = Connection::open(DB_NAME)?;
    conn.execute_batch(include_str!("../reset.sql"))?;

    Ok(())
}

pub fn reset_names() -> Result<(), Box<dyn Error>> {
    let mut conn = Connection::open(DB_NAME)?;

    let tx = conn.transaction()?;

    let games = {
        let mut stmt = tx
            .prepare("SELECT * FROM games ORDER BY timestamp ASC")
            .unwrap();

        let mut rows = stmt.query([]).unwrap();
        let mut games = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            games.push(Game::from_row(row));
        }
        games
    };

    for g in games {
        update_player(&tx, g.id_a, &g.name_a, g.game_floor);
        update_player(&tx, g.id_b, &g.name_b, g.game_floor);
    }

    tx.commit()?;

    Ok(())
}

pub fn reset_distribution() -> Result<(), Box<dyn Error>> {
    let mut conn = Connection::open(DB_NAME)?;

    update_player_distribution(&mut conn);

    Ok(())
}

pub fn load_json_data(path: &str) -> Result<(), Box<dyn Error>> {
    let mut conn = Connection::open(DB_NAME)?;

    #[derive(Deserialize)]
    #[allow(non_snake_case)]
    struct RawGame {
        time: String,
        floor: u32,
        winner: u32,
        playerAID: String,
        playerBID: String,
        playerAName: String,
        playerBName: String,
        playerACharCode: usize,
        playerBCharCode: usize,
    }

    for entry in glob(&format!("{}*.json", path)).unwrap() {
        let tx = conn.transaction().unwrap();
        match entry {
            Ok(path) => {
                info!("Loading replays from: {:?}", path);
                let file = File::open(path).unwrap();
                let reader = BufReader::new(file);
                let games: Vec<RawGame> = serde_json::from_reader(reader).unwrap();
                for g in games {
                    if g.time != "" {
                        let mut dt = g.time.split(' ');
                        let mut date = dt.next().unwrap().split('-');
                        let mut time = dt.next().unwrap().split(':');
                        let timestamp = NaiveDate::from_ymd(
                            date.next().unwrap().parse().unwrap(),
                            date.next().unwrap().parse().unwrap(),
                            date.next().unwrap().parse().unwrap(),
                        )
                        .and_hms(
                            time.next().unwrap().parse().unwrap(),
                            time.next().unwrap().parse().unwrap(),
                            time.next().unwrap().parse().unwrap(),
                        );
                        let timestamp = DateTime::<Utc>::from_utc(timestamp, Utc);
                        add_game(
                            &tx,
                            ggst_api::Match {
                                timestamp,
                                floor: ggst_api::Floor::from_u8(g.floor as u8).unwrap(),
                                winner: match g.winner {
                                    1 => ggst_api::Winner::Player1,
                                    2 => ggst_api::Winner::Player2,
                                    _ => panic!("Bad winner"),
                                },
                                players: (
                                    ggst_api::Player {
                                        id: g.playerAID.parse().unwrap(),
                                        character: ggst_api::Character::from_u8(
                                            g.playerACharCode as u8,
                                        )
                                        .unwrap(),
                                        name: g.playerAName.clone(),
                                    },
                                    ggst_api::Player {
                                        id: g.playerBID.parse().unwrap(),
                                        character: ggst_api::Character::from_u8(
                                            g.playerBCharCode as u8,
                                        )
                                        .unwrap(),
                                        name: g.playerBName.clone(),
                                    },
                                ),
                            },
                        )
                    }
                }
            }
            Err(e) => error!("Couldn't read path: {:?}", e),
        }
        tx.commit().unwrap();
    }

    Ok(())
}

pub async fn run() {
    try_join! {
        tokio::spawn(pull_continuous()),
        tokio::spawn(update_ratings_continuous()),
    }
    .unwrap();
}

async fn pull_continuous() {
    let mut conn = Connection::open(DB_NAME).unwrap();
    grab_games(&mut conn, 100).await;
    let mut interval = time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        grab_games(&mut conn, 10).await;
    }
}

pub async fn update_ratings_continuous() {
    let mut conn = Connection::open(DB_NAME).unwrap();

    calc_versus_matchups(&mut conn);

    let mut last_rating_timestmap: i64 = conn
        .query_row("SELECT (last_update) FROM config", [], |r| r.get(0))
        .unwrap();

    let mut interval = time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        while Utc::now().timestamp() - last_rating_timestmap > RATING_PERIOD + 60 {
            last_rating_timestmap = update_ratings(&mut conn);
            update_player_distribution(&mut conn);
            calc_versus_matchups(&mut conn);
        }
    }
}

pub async fn pull() {
    let mut conn = Connection::open(DB_NAME).unwrap();

    grab_games(&mut conn, 100).await;
}

async fn grab_games(conn: &mut Connection, pages: usize) {
    let replays = ggst_api::get_replays(
        &ggst_api::Context::default(),
        pages,
        127,
        ggst_api::Floor::F1,
        ggst_api::Floor::Celestial,
    )
    .await
    .unwrap();

    let (replays, errors): (Vec<_>, Vec<_>) = (replays.0.collect(), replays.1.collect());

    let old_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM games", [], |r| r.get(0))
        .unwrap();

    let tx = conn.transaction().unwrap();
    for r in &replays {
        add_game(&tx, r.clone());
    }

    tx.commit().unwrap();

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM games", [], |r| r.get(0))
        .unwrap();

    info!(
        "Grabbed {} games -  new games: {} ({} total)",
        replays.len(),
        count - old_count,
        count,
    );

    if count - old_count == replays.len() as i64 {
        error!("Only new replays! We're probably missing some, try increasing the page count.");
    } else if count - old_count > replays.len() as i64 / 2 {
        warn!("Over half the grabbed replays are new, consider increasing page count.");
    }

    if errors.len() > 0 {
        warn!("{} replays failed to parse!", errors.len());
    }
}

fn add_game(conn: &Transaction, game: ggst_api::Match) {
    let ggst_api::Match {
        timestamp,
        players: (a, b),
        floor: game_floor,
        winner,
    } = game;
    update_player(conn, a.id, &a.name, game_floor.to_u8() as i64);
    update_player(conn, b.id, &b.name, game_floor.to_u8() as i64);

    conn.execute(
        "INSERT OR IGNORE INTO games (
            timestamp, 
            id_a, 
            name_a,
            char_a,
            id_b,
            name_b,
            char_b,
            winner, 
            game_floor
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            timestamp.timestamp(),
            a.id,
            a.name,
            a.character.to_u8(),
            b.id,
            b.name,
            b.character.to_u8(),
            match winner {
                ggst_api::Winner::Player1 => 1,
                ggst_api::Winner::Player2 => 2,
            },
            game_floor.to_u8(),
        ],
    )
    .unwrap();
}

fn update_player(conn: &Transaction, id: i64, name: &str, floor: i64) {
    conn.execute(
        "REPLACE INTO players(id, name, floor) VALUES(?, ?, ?)",
        params![id, name, floor],
    )
    .unwrap();

    conn.execute(
        "INSERT OR IGNORE INTO player_names(id, name) VALUES(?, ?)",
        params![id, name],
    )
    .unwrap();
}

fn update_player_distribution(conn: &mut Connection) {
    let tx = conn.transaction().unwrap();

    tx.execute("DELETE FROM player_floor_distribution", [])
        .unwrap();
    tx.execute("DELETE FROM player_rating_distribution", [])
        .unwrap();

    for f in (1..=10).chain(std::iter::once(99)) {
        let player_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM players WHERE floor = ?",
                params![f],
                |r| r.get(0),
            )
            .unwrap();
        let game_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM games WHERE game_floor = ?",
                params![f],
                |r| r.get(0),
            )
            .unwrap();

        tx.execute(
            "INSERT INTO
            player_floor_distribution
            (floor, player_count, game_count) 
            VALUES (?, ?, ?)",
            params![f, player_count, game_count],
        )
        .unwrap();
    }

    for r in 0..60 {
        let r_min = r * 50;
        let r_max = (r + 1) * 50;

        let player_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) 
                FROM player_ratings 
                WHERE value >= ? AND value < ? AND deviation < ?",
                params![
                    glicko_to_glicko2(r_min as f64),
                    glicko_to_glicko2(r_max as f64),
                    MAX_DEVIATION
                ],
                |r| r.get(0),
            )
            .unwrap();

        if player_count < 10 {
            continue;
        }

        let player_count_cum: i64 = tx
            .query_row(
                "SELECT COUNT(*) 
                FROM player_ratings 
                WHERE value < ? AND deviation < ?",
                params![glicko_to_glicko2(r_max as f64), MAX_DEVIATION],
                |r| r.get(0),
            )
            .unwrap();

        tx.execute(
            "INSERT INTO
            player_rating_distribution
            (min_rating, max_rating, player_count, player_count_cum)
            VALUES (?, ?, ?, ?)",
            params![r_min, r_max, player_count, player_count_cum],
        )
        .unwrap();
    }

    tx.commit().unwrap();
}

fn update_ratings(conn: &mut Connection) -> i64 {
    let last_timestamp: i64 = conn
        .query_row("SELECT last_update FROM config", [], |r| r.get(0))
        .unwrap();
    let next_timestamp = last_timestamp + RATING_PERIOD;

    info!(
        "Calculating ratings between {} and {}...",
        NaiveDateTime::from_timestamp(last_timestamp, 0).format("%Y-%m-%d %H:%M"),
        NaiveDateTime::from_timestamp(next_timestamp, 0).format("%Y-%m-%d %H:%M")
    );

    //Fetch the games from the rating period
    let games = {
        let mut stmt = conn
            .prepare("SELECT * FROM games WHERE timestamp >= ? AND timestamp < ?")
            .unwrap();

        let mut rows = stmt.query([last_timestamp, next_timestamp]).unwrap();
        let mut games = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            games.push(Game::from_row(row));
        }
        games
    };

    //Fetch all our rated players
    let mut players = {
        let mut players = FxHashMap::default();

        let mut stmt = conn
            .prepare(
                "SELECT 
                    id, char_id, wins, losses, value, deviation, volatility 
                FROM player_ratings",
            )
            .unwrap();
        let mut rows = stmt.query([]).unwrap();

        while let Some(row) = rows.next().unwrap() {
            let player = RatedPlayer::from_row(row);
            players.insert(
                (player.id, player.char_id),
                (player, Vec::<GameResult>::new()),
            );
        }
        players
    };

    let tx = conn.transaction().unwrap();

    for g in games {
        update_player(&tx, g.id_a, &g.name_a, g.game_floor);
        update_player(&tx, g.id_b, &g.name_b, g.game_floor);

        let rating_a = players
            .entry((g.id_a, g.char_a))
            .or_insert((RatedPlayer::new(g.id_a, g.char_a), Vec::new()))
            .0
            .rating;

        let rating_b = players
            .entry((g.id_b, g.char_b))
            .or_insert((RatedPlayer::new(g.id_b, g.char_b), Vec::new()))
            .0
            .rating;

        let a_win_prob = rating_a.value.exp() / (rating_a.value.exp() + rating_b.value.exp());
        let b_win_prob = 1.0 - a_win_prob;

        tx.execute(
            "INSERT OR IGNORE INTO player_matchups VALUES(?, ?, ?, 0, 0, 0, 0)",
            params![g.id_a, g.char_a, g.char_b,],
        )
        .unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO player_matchups VALUES(?, ?, ?, 0, 0, 0, 0)",
            params![g.id_b, g.char_b, g.char_a,],
        )
        .unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO global_matchups VALUES(?, ?, 0, 0, 0, 0)",
            params![g.char_a, g.char_b,],
        )
        .unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO global_matchups VALUES(?, ?, 0, 0, 0, 0)",
            params![g.char_b, g.char_a,],
        )
        .unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO high_rated_matchups VALUES(?, ?, 0, 0, 0, 0)",
            params![g.char_a, g.char_b,],
        )
        .unwrap();
        tx.execute(
            "INSERT OR IGNORE INTO high_rated_matchups VALUES(?, ?, 0, 0, 0, 0)",
            params![g.char_b, g.char_a,],
        )
        .unwrap();

        match g.winner {
            1 => {
                players
                    .get_mut(&(g.id_a, g.char_a))
                    .unwrap()
                    .1
                    .push(GameResult::win(rating_b));
                players
                    .get_mut(&(g.id_b, g.char_b))
                    .unwrap()
                    .1
                    .push(GameResult::loss(rating_a));
                players.get_mut(&(g.id_a, g.char_a)).unwrap().0.win_count += 1;
                players.get_mut(&(g.id_b, g.char_b)).unwrap().0.loss_count += 1;

                tx.execute(
                    "UPDATE player_matchups 
                    SET wins_real = wins_real + 1
                    WHERE id=? AND char_id=? AND opp_char_id=?",
                    params![g.id_a, g.char_a, g.char_b,],
                )
                .unwrap();
                tx.execute(
                    "UPDATE player_matchups 
                    SET losses_real = losses_real + 1
                    WHERE id=? AND char_id=? AND opp_char_id=?",
                    params![g.id_b, g.char_b, g.char_a,],
                )
                .unwrap();

                //TODO I know this is awful
                if rating_a.deviation < MAX_DEVIATION && rating_b.deviation < MAX_DEVIATION {
                    tx.execute(
                        "UPDATE player_matchups 
                        SET wins_adjusted = wins_adjusted + ?
                        WHERE id=? AND char_id=? AND opp_char_id=?",
                        params![b_win_prob, g.id_a, g.char_a, g.char_b,],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE player_matchups 
                        SET losses_adjusted = losses_adjusted + ?
                        WHERE id=? AND char_id=? AND opp_char_id=?",
                        params![b_win_prob, g.id_b, g.char_b, g.char_a,],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE global_matchups 
                        SET wins_real = wins_real + 1, wins_adjusted = wins_adjusted + ?
                        WHERE char_id=? AND opp_char_id=?",
                        params![b_win_prob, g.char_a, g.char_b,],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE global_matchups 
                        SET losses_real = losses_real + 1, losses_adjusted = losses_adjusted + ?
                        WHERE char_id=? AND opp_char_id=?",
                        params![b_win_prob, g.char_b, g.char_a,],
                    )
                    .unwrap();

                    if rating_a.value > HIGH_RATING && rating_b.value > HIGH_RATING {
                        tx.execute(
                            "UPDATE high_rated_matchups 
                            SET wins_real = wins_real + 1, wins_adjusted = wins_adjusted + ?
                            WHERE char_id=? AND opp_char_id=?",
                            params![b_win_prob, g.char_a, g.char_b,],
                        )
                        .unwrap();
                        tx.execute(
                            "UPDATE high_rated_matchups 
                            SET losses_real = losses_real + 1, losses_adjusted = losses_adjusted + ?
                            WHERE char_id=? AND opp_char_id=?",
                            params![b_win_prob, g.char_b, g.char_a,],
                        )
                        .unwrap();
                    }
                }
            }
            2 => {
                players
                    .get_mut(&(g.id_a, g.char_a))
                    .unwrap()
                    .1
                    .push(GameResult::loss(rating_b));
                players
                    .get_mut(&(g.id_b, g.char_b))
                    .unwrap()
                    .1
                    .push(GameResult::win(rating_a));
                players.get_mut(&(g.id_a, g.char_a)).unwrap().0.loss_count += 1;
                players.get_mut(&(g.id_b, g.char_b)).unwrap().0.win_count += 1;

                tx.execute(
                    "UPDATE player_matchups 
                    SET losses_real = losses_real + 1
                    WHERE id=? AND char_id=? AND opp_char_id=?",
                    params![g.id_a, g.char_a, g.char_b,],
                )
                .unwrap();

                tx.execute(
                    "UPDATE player_matchups 
                    SET wins_real = wins_real + 1
                    WHERE id=? AND char_id=? AND opp_char_id=?",
                    params![g.id_b, g.char_b, g.char_a,],
                )
                .unwrap();

                //TODO make this less repetitive
                if rating_a.deviation < MAX_DEVIATION && rating_b.deviation < MAX_DEVIATION {
                    tx.execute(
                        "UPDATE player_matchups 
                        SET losses_adjusted = losses_adjusted + ?
                        WHERE id=? AND char_id=? AND opp_char_id=?",
                        params![a_win_prob, g.id_a, g.char_a, g.char_b,],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE player_matchups 
                        SET wins_adjusted = wins_adjusted + ?
                        WHERE id=? AND char_id=? AND opp_char_id=?",
                        params![a_win_prob, g.id_b, g.char_b, g.char_a,],
                    )
                    .unwrap();

                    tx.execute(
                        "UPDATE global_matchups 
                        SET wins_real = wins_real + 1, wins_adjusted = wins_adjusted + ?
                        WHERE char_id=? AND opp_char_id=?",
                        params![a_win_prob, g.char_b, g.char_a,],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE global_matchups 
                        SET losses_real = losses_real + 1, losses_adjusted = losses_adjusted + ?
                        WHERE char_id=? AND opp_char_id=?",
                        params![a_win_prob, g.char_a, g.char_b,],
                    )
                    .unwrap();

                    if rating_a.value > HIGH_RATING && rating_b.value > HIGH_RATING {
                        tx.execute(
                            "UPDATE high_rated_matchups 
                            SET wins_real = wins_real + 1, wins_adjusted = wins_adjusted + ?
                            WHERE char_id=? AND opp_char_id=?",
                            params![b_win_prob, g.char_b, g.char_a,],
                        )
                        .unwrap();
                        tx.execute(
                            "UPDATE high_rated_matchups 
                            SET losses_real = losses_real + 1, losses_adjusted = losses_adjusted + ?
                            WHERE char_id=? AND opp_char_id=?",
                            params![b_win_prob, g.char_a, g.char_b,],
                        )
                        .unwrap();
                    }
                }
            }
            _ => panic!("Bad winner"),
        }

        tx.execute(
            "INSERT INTO game_ratings VALUES(?, ?, ?, ?, ?, ?, ?)",
            params![
                g.timestamp,
                g.id_a,
                rating_a.value,
                rating_a.deviation,
                g.id_b,
                rating_b.value,
                rating_b.deviation,
            ],
        )
        .unwrap();

        //TODO add to player_matchup
    }

    for (_, (mut player, results)) in players.into_iter() {
        player.rating = glicko2::new_rating(player.rating, &results, SYS_CONSTANT);

        if player.rating.deviation < 0.0 {
            error!("Negative rating deviation???");
        }

        tx.execute(
            "REPLACE INTO player_ratings VALUES(?, ?, ?, ?, ?, ?, ?)",
            params![
                player.id,
                player.char_id,
                player.win_count,
                player.loss_count,
                player.rating.value,
                player.rating.deviation,
                player.rating.volatility,
            ],
        )
        .unwrap();
    }

    tx.execute("UPDATE config SET last_update=?", [next_timestamp])
        .unwrap();

    tx.commit().unwrap();

    next_timestamp
}

pub fn calc_versus_matchups(conn: &mut Connection) {
    let mut pairs = FxHashMap::<((i64, i64), (i64, i64)), (f64, f64, i64)>::default();
    info!("Calculating matchups");

    {
        let mut stmt = conn
            .prepare(
                "SELECT
            id_a, char_a, value_a, id_b, char_b, value_b, winner
            FROM games NATURAL JOIN game_ratings
            WHERE value_a > ? AND deviation_a < ? AND value_b > ? AND deviation_b < ?;",
            )
            .unwrap();

        let mut rows = stmt
            .query(params![
                HIGH_RATING,
                MAX_DEVIATION,
                HIGH_RATING,
                MAX_DEVIATION
            ])
            .unwrap();

        while let Some(row) = rows.next().unwrap() {
            let id_a: i64 = row.get(0).unwrap();
            let char_a: i64 = row.get(1).unwrap();
            let value_a: f64 = row.get(2).unwrap();
            let id_b: i64 = row.get(3).unwrap();
            let char_b: i64 = row.get(4).unwrap();
            let value_b: f64 = row.get(5).unwrap();
            let winner: i64 = row.get(6).unwrap();

            if let Some((a, b, v_a, v_b, winner)) = {
                if char_a < char_b {
                    Some(((id_a, char_a), (id_b, char_b), value_a, value_b, winner))
                } else if char_b < char_a {
                    Some((
                        (id_b, char_b),
                        (id_a, char_a),
                        value_b,
                        value_a,
                        if winner == 1 { 2 } else { 1 },
                    ))
                } else {
                    None
                }
            } {
                let p = pairs.entry((a, b)).or_insert((0.0, 0.0, 0));
                let win_chance = v_a.exp() / (v_a.exp() + v_b.exp());
                let loss_chance = 1.0 - win_chance;

                match winner {
                    1 => {
                        p.0 += loss_chance;
                    }
                    2 => {
                        p.1 += win_chance;
                    }
                    _ => panic!("Bad winner"),
                }
                p.2 += 1;
            }
        }
    }

    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM versus_matchups", []).unwrap();

    for a in 0..website::CHAR_NAMES.len() - 1 {
        for b in (a + 1)..website::CHAR_NAMES.len() {
            let a = a as i64;
            let b = b as i64;
            let i = pairs
                .iter()
                .filter(|(((_, c_a), (_, c_b)), _)| *c_a == a && *c_b == b);
            let sum: f64 = i
                .clone()
                .map(|(_, (wins, losses, _))| wins / (wins + losses))
                .sum();
            let pair_count = i.clone().count();
            let game_count: i64 = i.clone().map(|(_, (_, _, games))| games).sum();
            let probability = sum / pair_count as f64;
            tx.execute(
                "INSERT INTO 
                versus_matchups(char_a, char_b, game_count, pair_count, win_rate)
                VALUES(?, ?, ?, ?, ?)",
                params![a, b, game_count, pair_count, probability],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO 
                versus_matchups(char_a, char_b, game_count, pair_count, win_rate)
                VALUES(?, ?, ?, ?, ?)",
                params![b, a, game_count, pair_count, 1.0 - probability],
            )
            .unwrap();
        }
    }

    tx.commit().unwrap();

    info!("Done");
}

pub struct Game {
    timestamp: i64,
    id_a: i64,
    name_a: String,
    char_a: i64,
    id_b: i64,
    name_b: String,
    char_b: i64,
    winner: i64,
    game_floor: i64,
}

impl Game {
    pub fn from_row(row: &Row) -> Self {
        Self {
            timestamp: row.get(0).unwrap(),
            id_a: row.get(1).unwrap(),
            name_a: row.get(2).unwrap(),
            char_a: row.get(3).unwrap(),
            id_b: row.get(4).unwrap(),
            name_b: row.get(5).unwrap(),
            char_b: row.get(6).unwrap(),
            winner: row.get(7).unwrap(),
            game_floor: row.get(8).unwrap(),
        }
    }
}

pub struct RatedPlayer {
    pub id: i64,
    pub char_id: i64,
    pub win_count: i64,
    pub loss_count: i64,
    pub rating: Glicko2Rating,
}

impl RatedPlayer {
    pub fn new(id: i64, char_id: i64) -> Self {
        Self {
            id,
            char_id,
            win_count: 0,
            loss_count: 0,
            rating: Glicko2Rating::unrated(),
        }
    }
    pub fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0).unwrap(),
            char_id: row.get(1).unwrap(),
            win_count: row.get(2).unwrap(),
            loss_count: row.get(3).unwrap(),
            rating: Glicko2Rating {
                value: row.get(4).unwrap(),
                deviation: row.get(5).unwrap(),
                volatility: row.get(6).unwrap(),
            },
        }
    }
}
