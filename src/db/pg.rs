use log::{debug, error, info, trace};

use super::*;
use crate::prelude::*;
use dotenv::dotenv;
use failure::format_err;
use postgres::{transaction::Transaction, Connection, TlsMode};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{env, fmt::Write, str::FromStr};

pub fn establish_connection() -> Result<Connection> {
    dotenv()?;

    let database_url = env::var("DATABASE_URL")?;
    Ok(Connection::connect(database_url, TlsMode::None)?)
}

fn create_bulk_insert_blocks_query(blocks: &[Block]) -> Vec<String> {
    if blocks.is_empty() {
        return vec![];
    }
    if blocks.len() > 9000 {
        let mid = blocks.len() / 2;
        let mut p1 = create_bulk_insert_blocks_query(&blocks[0..mid]);
        let mut p2 = create_bulk_insert_blocks_query(&blocks[mid..blocks.len()]);
        p1.append(&mut p2);
        return p1;
    }

    let mut q: String = "INSERT INTO blocks (height, hash, prev_hash) VALUES".into();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            q.push_str(",")
        }
        q.write_fmt(format_args!(
            "({}, '\\x{}', '\\x{}')",
            block.height, block.hash, block.prev_hash,
        ))
        .unwrap();
    }
    q.write_str(";");
    return vec![q];
}

fn create_bulk_insert_txs_query(txs: &[Tx]) -> Vec<String> {
    if txs.is_empty() {
        return vec![];
    }
    if txs.len() > 9000 {
        let mid = txs.len() / 2;
        let mut p1 = create_bulk_insert_txs_query(&txs[0..mid]);
        let mut p2 = create_bulk_insert_txs_query(&txs[mid..txs.len()]);
        p1.append(&mut p2);
        return p1;
    }

    let mut q: String = "INSERT INTO txs (height, hash, coinbase) VALUES".into();
    for (i, tx) in txs.iter().enumerate() {
        if i > 0 {
            q.push_str(",")
        }
        q.write_fmt(format_args!(
            "({},'\\x{}',{})",
            tx.height, tx.hash, tx.coinbase,
        ))
        .unwrap();
    }
    q.write_str(";");
    return vec![q];
}

fn create_bulk_insert_outputs_query(
    outputs: &[Output],
    tx_ids: &HashMap<TxHash, i64>,
) -> Vec<String> {
    if outputs.is_empty() {
        return vec![];
    }
    if outputs.len() > 9000 {
        let mid = outputs.len() / 2;
        let mut p1 = create_bulk_insert_outputs_query(&outputs[0..mid], tx_ids);
        let mut p2 = create_bulk_insert_outputs_query(&outputs[mid..outputs.len()], tx_ids);
        p1.append(&mut p2);
        return p1;
    }

    let mut q: String =
        "INSERT INTO outputs (height, tx_id, tx_idx, value, address, coinbase) VALUES ".into();
    for (i, output) in outputs.iter().enumerate() {
        if i > 0 {
            q.push_str(",")
        }
        q.write_fmt(format_args!(
            "({},{},{},{},{},{})",
            output.height,
            tx_ids[&output.out_point.txid],
            output.out_point.vout,
            output.value,
            output
                .address
                .as_ref()
                .map_or("null".into(), |s| format!("'{}'", s)),
            output.coinbase,
        ))
        .unwrap();
    }
    q.write_str(";");
    return vec![q];
}

fn create_bulk_insert_inputs_query(
    inputs: &[Input],
    outputs: &HashMap<OutPoint, UtxoSetEntry>,
) -> Vec<String> {
    if inputs.is_empty() {
        return vec![];
    }
    if inputs.len() > 9000 {
        let mid = inputs.len() / 2;
        let mut p1 = create_bulk_insert_inputs_query(&inputs[0..mid], outputs);
        let mut p2 = create_bulk_insert_inputs_query(&inputs[mid..inputs.len()], outputs);
        p1.append(&mut p2);
        return p1;
    }

    let mut q: String = "INSERT INTO inputs (height, output_id) VALUES ".into();
    for (i, input) in inputs.iter().enumerate() {
        if i > 0 {
            q.push_str(",")
        }
        q.write_fmt(format_args!(
            "({},{})",
            input.height, outputs[&input.out_point].id,
        ))
        .unwrap();
    }
    q.write_str(";");
    vec![q]
}

fn crate_fetch_outputs_query(outputs: &[OutPoint]) -> Vec<String> {
    if outputs.len() > 1500 {
        let mid = outputs.len() / 2;
        let mut p1 = crate_fetch_outputs_query(&outputs[0..mid]);
        let mut p2 = crate_fetch_outputs_query(&outputs[mid..outputs.len()]);
        p1.append(&mut p2);
        return p1;
    }
    let mut q: String = "SELECT outputs.id, outputs.value, txs.hash, outputs.tx_idx FROM outputs INNER JOIN txs ON (txs.id = outputs.tx_id) WHERE (txs.hash, outputs.tx_idx) IN ( VALUES ".into();
    for (i, output) in outputs.iter().enumerate() {
        if i > 0 {
            q.push_str(",")
        }
        q.write_fmt(format_args!(
            "('\\x{}'::bytea,{})",
            output.txid, output.vout
        ))
        .unwrap();
    }
    q.write_str(" );");
    vec![q]
}

#[derive(Copy, Clone, PartialEq, Eq)]
struct UtxoSetEntry {
    id: i64,
    value: u64,
}

#[derive(Default)]
/// Cache of utxo set
struct UtxoSetCache {
    entries: HashMap<OutPoint, UtxoSetEntry>,
}

impl UtxoSetCache {
    fn insert(&mut self, point: OutPoint, id: i64, value: u64) {
        self.entries.insert(point, UtxoSetEntry { id, value });
    }

    /// Consume `outputs`
    ///
    /// Returns:
    /// * Mappings for Outputs that were found
    /// * Vector of outputs that were missing from the set
    fn consume(
        &mut self,
        outputs: impl Iterator<Item = OutPoint>,
    ) -> (HashMap<OutPoint, UtxoSetEntry>, Vec<OutPoint>) {
        let mut found = HashMap::default();
        let mut missing = vec![];

        for output in outputs {
            match self.entries.remove(&output) {
                Some(details) => {
                    found.insert(output, details);
                }
                None => missing.push(output),
            }
        }

        (found, missing)
    }

    fn fetch_missing(
        conn: &Connection,
        missing: Vec<OutPoint>,
    ) -> Result<HashMap<OutPoint, UtxoSetEntry>> {
        let missing_len = missing.len();
        debug!("Fetching {} missing outputs", missing_len);
        let mut out = HashMap::default();

        if missing.is_empty() {
            return Ok(HashMap::default());
        }

        let start = Instant::now();
        let missing: Vec<_> = missing.into_iter().collect();
        for q in crate_fetch_outputs_query(&missing) {
            for row in &conn.query(&q, &[])? {
                let tx_hash = {
                    let mut human_bytes = row.get::<_, Vec<u8>>(2);
                    human_bytes.reverse();
                    BlockHash::from(human_bytes.as_slice())
                };
                out.insert(
                    OutPoint {
                        txid: tx_hash,
                        vout: row.get::<_, i32>(3) as u32,
                    },
                    UtxoSetEntry {
                        id: row.get(0),
                        value: row.get::<_, i64>(1) as u64,
                    },
                );
            }
        }

        trace!(
            "Fetched {} missing outputs in {}s",
            missing_len,
            Instant::now().duration_since(start).as_secs()
        );
        Ok(out)
    }
}

fn read_next_id(conn: &Connection, table_name: &str, id_col_name: &str) -> Result<i64> {
    // explanation: https://dba.stackexchange.com/a/78228
    let q = format!(
        "select setval(pg_get_serial_sequence('{table}', '{id_col}'), GREATEST(nextval(pg_get_serial_sequence('{table}', '{id_col}')) - 1, 1)) as id",
        table = table_name,
        id_col = id_col_name
    );
    const PG_STARTING_ID: i64 = 1;
    Ok(conn
        .query(&q, &[])?
        .iter()
        .next()
        .expect("at least one row")
        .get::<_, Option<i64>>(0)
        .map(|v| v + 1)
        .unwrap_or(PG_STARTING_ID))
}

fn execute_bulk_insert_transcation(
    conn: &Connection,
    name: &str,
    len: usize,
    batch_id: u64,
    queries: impl Iterator<Item = String>,
) -> Result<()> {
    trace!("Inserting {} {} from batch {}...", len, name, batch_id);
    let start = Instant::now();
    let transaction = conn.transaction()?;
    for s in queries {
        transaction.batch_execute(&s)?;
    }
    transaction.commit()?;
    trace!(
        "Inserted {} {} from batch {} in {}s",
        len,
        name,
        batch_id,
        Instant::now().duration_since(start).as_secs()
    );
    Ok(())
}

fn read_next_tx_id(conn: &Connection) -> Result<i64> {
    read_next_id(conn, "txs", "id")
}

fn read_next_output_id(conn: &Connection) -> Result<i64> {
    read_next_id(conn, "outputs", "id")
}

fn read_next_block_id(conn: &Connection) -> Result<i64> {
    read_next_id(conn, "blocks", "id")
}

type BlocksInFlight = HashMap<BlockHeight, BlockHash>;

/// Worker Pipepline
///
/// `Pipeline` is reponsible for actually inserting data into the db.
///  It is split between multiple threads - each handling one table.
///  The idea here is to have some level of paralelism to help saturate
///  network IO, and then disk IO. As each thread touches only one
///  table - there is no contention between them.
///
///  `Pipeline` name comes from the fact that each thread does its job
///  and passes rest of the data to the next one.
///
///  A lot of stuff here about performance is actually speculative,
///  but there is only so many hours in a day, and it seems to work well
///  in practice.
///
///  In as `atomic` mode, last thread inserts entire data in one transaction
///  to prevent temporary inconsistency (eg. txs inserted, but blocks not yet).
///  It is to be used in non-bulk mode, when blocks are indexed one at the time,
///  so performance is not important. Passing formatted queries around is a compromise
///  between having two different versions of this logic, and good performance
///  in bulk mode.
struct Pipeline {
    in_flight: Arc<Mutex<BlocksInFlight>>,
    tx: Option<crossbeam_channel::Sender<(u64, Vec<Parsed>)>>,
    txs_thread: Option<std::thread::JoinHandle<Result<()>>>,
    outputs_thread: Option<std::thread::JoinHandle<Result<()>>>,
    inputs_thread: Option<std::thread::JoinHandle<Result<()>>>,
    blocks_thread: Option<std::thread::JoinHandle<Result<()>>>,
}

// TODO: fail the whole Pipeline somehow
fn fn_log_err<F>(name: &'static str, mut f: F) -> impl FnMut() -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    move || {
        let res = f();
        if let Err(ref e) = res {
            error!("{} finished with an error: {}", name, e);
        }

        res
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PipelineMode {
    Atomic,
    Bulk,
}

impl PipelineMode {
    fn is_atomic(self) -> bool {
        self == PipelineMode::Atomic
    }
}

impl Pipeline {
    fn new(in_flight: Arc<Mutex<BlocksInFlight>>, mode: PipelineMode) -> Result<Self> {
        /// We use only rendezvous (0-size) channels, to allow passing
        /// work and parallelism, but without doing any buffering of
        /// work in the channels. Buffered work does not
        /// improve performance, and  more things in flight means
        /// incrased memory usage.
        let (tx, txs_rx) = crossbeam_channel::bounded::<(u64, Vec<Parsed>)>(0);
        let (txs_tx, outputs_rx) = crossbeam_channel::bounded::<(
            u64,
            Vec<Block>,
            Vec<Output>,
            Vec<Input>,
            HashMap<TxHash, i64>,
            Vec<Vec<String>>,
        )>(0);
        let (outputs_tx, inputs_rx) =
            crossbeam_channel::bounded::<(u64, Vec<Block>, Vec<Input>, Vec<Vec<String>>)>(0);

        let (inputs_tx, blocks_rx) =
            crossbeam_channel::bounded::<(u64, Vec<Block>, Vec<Vec<String>>)>(0);
        let utxo_set_cache = Arc::new(Mutex::new(UtxoSetCache::default()));

        let txs_thread = std::thread::spawn({
            let conn = establish_connection()?;
            fn_log_err("db_worker_txs", move || {
                let mut next_id = read_next_tx_id(&conn)?;
                while let Ok((batch_id, parsed)) = txs_rx.recv() {
                    assert_eq!(next_id, read_next_tx_id(&conn)?);

                    let mut blocks: Vec<super::Block> = vec![];
                    let mut txs: Vec<super::Tx> = vec![];
                    let mut outputs: Vec<super::Output> = vec![];
                    let mut inputs: Vec<super::Input> = vec![];
                    let mut pending_queries = vec![];

                    for mut parsed in parsed {
                        blocks.push(parsed.block);
                        txs.append(&mut parsed.txs);
                        outputs.append(&mut parsed.outputs);
                        inputs.append(&mut parsed.inputs);
                    }

                    let queries = create_bulk_insert_txs_query(&txs);
                    if mode.is_atomic() {
                        pending_queries.push(queries);
                    } else {
                        execute_bulk_insert_transcation(
                            &conn,
                            "txs",
                            txs.len(),
                            batch_id,
                            queries.into_iter(),
                        )?
                    };

                    let batch_len = txs.len();
                    let tx_ids: HashMap<_, _> = txs
                        .into_iter()
                        .enumerate()
                        .map(|(i, tx)| (tx.hash, next_id + i as i64))
                        .collect();

                    next_id += batch_len as i64;

                    txs_tx.send((batch_id, blocks, outputs, inputs, tx_ids, pending_queries))?;
                }
                Ok(())
            })
        });

        let outputs_thread = std::thread::spawn({
            let conn = establish_connection()?;
            let utxo_set_cache = utxo_set_cache.clone();
            fn_log_err("db_worker_outputs", move || {
                let mut next_id = read_next_output_id(&conn)?;
                while let Ok((batch_id, blocks, outputs, inputs, tx_ids, mut pending_queries)) =
                    outputs_rx.recv()
                {
                    assert_eq!(next_id, read_next_output_id(&conn)?);

                    let queries = create_bulk_insert_outputs_query(&outputs, &tx_ids);

                    if mode.is_atomic() {
                        pending_queries.push(queries);
                    } else {
                        execute_bulk_insert_transcation(
                            &conn,
                            "outputs",
                            outputs.len(),
                            batch_id,
                            queries.into_iter(),
                        )?;
                    }

                    let mut utxo_lock = utxo_set_cache.lock().unwrap();
                    outputs.iter().enumerate().for_each(|(i, output)| {
                        let id = next_id + (i as i64);
                        utxo_lock.insert(output.out_point, id, output.value);
                    });
                    drop(utxo_lock);

                    next_id += outputs.len() as i64;

                    outputs_tx.send((batch_id, blocks, inputs, pending_queries))?;
                }
                Ok(())
            })
        });

        let inputs_thread = std::thread::spawn({
            let conn = establish_connection()?;
            let utxo_set_cache = utxo_set_cache.clone();
            fn_log_err("db_worker_inputs", move || {
                while let Ok((batch_id, blocks, inputs, mut pending_queries)) = inputs_rx.recv() {
                    let mut utxo_lock = utxo_set_cache.lock().unwrap();
                    let (mut output_ids, missing) =
                        utxo_lock.consume(inputs.iter().map(|i| i.out_point));
                    drop(utxo_lock);
                    let missing = UtxoSetCache::fetch_missing(&conn, missing)?;
                    for (k, v) in missing.into_iter() {
                        output_ids.insert(k, v);
                    }

                    let queries = create_bulk_insert_inputs_query(&inputs, &output_ids);
                    if mode.is_atomic() {
                        pending_queries.push(queries);
                    } else {
                        execute_bulk_insert_transcation(
                            &conn,
                            "inputs",
                            inputs.len(),
                            batch_id,
                            queries.into_iter(),
                        )?;
                    }

                    inputs_tx.send((batch_id, blocks, pending_queries))?;
                }
                Ok(())
            })
        });

        let blocks_thread = std::thread::spawn({
            let conn = establish_connection()?;
            let in_flight = in_flight.clone();
            fn_log_err("db_worker_blocks", move || {
                while let Ok((batch_id, blocks, mut pending_queries)) = blocks_rx.recv() {
                    let queries = create_bulk_insert_blocks_query(&blocks);

                    if mode.is_atomic() {
                        pending_queries.push(queries);

                        execute_bulk_insert_transcation(
                            &conn,
                            "all block data",
                            blocks.len(),
                            batch_id,
                            pending_queries.into_iter().flatten(),
                        )?;
                    } else {
                        execute_bulk_insert_transcation(
                            &conn,
                            "blocks",
                            blocks.len(),
                            batch_id,
                            queries.into_iter(),
                        )?;
                    }
                    info!(
                        "Block {}H fully indexed and commited",
                        blocks
                            .iter()
                            .rev()
                            .next()
                            .map(|b| b.height)
                            .expect("at least one block")
                    );
                    let mut any_missing = false;
                    let mut lock = in_flight.lock().unwrap();
                    for block in &blocks {
                        any_missing = any_missing || lock.remove(&block.height).is_none();
                    }
                    drop(lock);
                    assert!(!any_missing);
                }
                Ok(())
            })
        });
        Ok(Self {
            in_flight,
            tx: Some(tx),
            txs_thread: Some(txs_thread),
            outputs_thread: Some(outputs_thread),
            inputs_thread: Some(inputs_thread),
            blocks_thread: Some(blocks_thread),
        })
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        drop(self.tx.take());

        let joins = vec![
            self.txs_thread.take().unwrap(),
            self.outputs_thread.take().unwrap(),
            self.inputs_thread.take().unwrap(),
            self.blocks_thread.take().unwrap(),
        ];

        for join in joins {
            join.join().expect("Worker thread panicked");
        }
    }
}

pub struct Postresql {
    connection: Connection,
    cached_max_height: Option<u64>,
    pipeline: Option<Pipeline>,
    batch: Vec<BlockInfo>,
    batch_txs_total: u64,
    batch_id: u64,
    bulk_mode: bool,

    in_flight: Arc<Mutex<BlocksInFlight>>,
}

impl Drop for Postresql {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

impl Postresql {
    pub fn new() -> Result<Self> {
        let connection = establish_connection()?;
        let mut s = Postresql {
            connection,
            pipeline: None,
            cached_max_height: None,
            batch: vec![],
            batch_txs_total: 0,
            batch_id: 0,
            bulk_mode: true,
            in_flight: Arc::new(Mutex::new(BlocksInFlight::new())),
        };
        s.init()?;
        s.wipe_inconsistent_data()?;
        s.start_workers();
        Ok(s)
    }

    fn init(&mut self) -> Result<()> {
        info!("Creating db schema");
        self.connection
            .batch_execute(include_str!("pg_init_base.sql"))?;
        Ok(())
    }

    /// Wipe all records with `> height` from table sorted by id
    ///
    /// We can use the fact that both `ids` (primary keys) and `height`
    /// are growing monotonicially, and delete all records with `height`
    /// greather than a given argument without having a index on `height`
    /// itself, by using taking a fixed chunk with highest `id` and deleting
    /// by `height` from it.
    fn wipe_gt_height_from_id_sorted_table(
        &mut self,
        table: &str,
        id_col_name: &str,
        height: BlockHeight,
    ) -> Result<()> {
        debug!("Wiping {} higher than {}H", table, height);
        let start = Instant::now();
        let q = format!("DELETE FROM {table} WHERE {id_col} > (SELECT max({id_col}) - 100000 FROM {table}) AND height > $1;",
                       table = table, id_col = id_col_name);

        let mut total = 0;
        loop {
            let deleted = self.connection.execute(&q, &[&(height as i64)])?;
            total += deleted;
            if deleted == 0 {
                break;
            }
        }

        trace!(
            "Wiped {} records from {} in {}s",
            total,
            table,
            Instant::now().duration_since(start).as_secs()
        );
        Ok(())
    }

    fn wipe_txs_higher_than_height(&mut self, height: BlockHeight) -> Result<()> {
        self.wipe_gt_height_from_id_sorted_table("txs", "id", height)
    }

    fn wipe_inputs_higher_than_height(&mut self, height: BlockHeight) -> Result<()> {
        self.wipe_gt_height_from_id_sorted_table("inputs", "output_id", height)
    }

    fn wipe_outputs_higher_than_height(&mut self, height: BlockHeight) -> Result<()> {
        self.wipe_gt_height_from_id_sorted_table("outputs", "id", height)
    }

    /// Wipe all the data that might have been added, before a `block` entry
    /// was commited to the DB.
    ///
    /// `Blocks` is  the last table to have data inserted, and is
    /// used as a commitment that everything else was inserted already..
    fn wipe_inconsistent_data(&mut self) -> Result<()> {
        if let Some(height) = self.get_max_height()? {
            self.wipe_txs_higher_than_height(height)?;
            self.wipe_outputs_higher_than_height(height)?;
            self.wipe_inputs_higher_than_height(height)?;
        }

        Ok(())
    }

    fn stop_workers(&mut self) {
        debug!("Stopping DB pipeline workers");
        self.pipeline.take();
        assert!(self.in_flight.lock().unwrap().is_empty());
    }

    fn start_workers(&mut self) {
        debug!("Starting DB pipeline workers");
        // TODO: This `unwrap` is not OK. Connecting to db can fail.
        self.pipeline = Some(
            Pipeline::new(
                self.in_flight.clone(),
                if self.bulk_mode {
                    PipelineMode::Bulk
                } else {
                    PipelineMode::Atomic
                },
            )
            .unwrap(),
        )
    }

    fn flush_workers(&mut self) {
        self.stop_workers();
        self.start_workers();
    }

    fn update_max_height(&mut self, info: &BlockInfo) {
        self.cached_max_height = Some(
            self.cached_max_height
                .map_or(info.height, |h| std::cmp::max(h, info.height)),
        );
    }

    fn flush_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        trace!(
            "Flushing batch {}, with {} txes",
            self.batch_id,
            self.batch_txs_total
        );
        let parsed: Result<Vec<_>> = std::mem::replace(&mut self.batch, vec![])
            .par_iter()
            .map(|block_info| super::parse_node_block(&block_info))
            .collect();
        let parsed = parsed?;

        let mut in_flight = self.in_flight.lock().expect("locking works");
        for parsed in &parsed {
            in_flight.insert(parsed.block.height, parsed.block.hash);
        }
        drop(in_flight);

        self.pipeline
            .as_ref()
            .expect("workers running")
            .tx
            .as_ref()
            .expect("tx not null")
            .send((self.batch_id, parsed));
        trace!("Batch flushed");
        self.batch_txs_total = 0;
        self.batch_id += 1;
        Ok(())
    }
}

impl DataStore for Postresql {
    fn wipe(&mut self) -> Result<()> {
        info!("Wiping db schema");
        self.connection.batch_execute(include_str!("pg_wipe.sql"))?;
        Ok(())
    }

    fn mode_bulk(&mut self) -> Result<()> {
        info!("Entering bulk mode: minimum indices");
        self.bulk_mode = true;
        self.connection
            .batch_execute(include_str!("pg_mode_bulk.sql"))?;
        Ok(())
    }

    fn mode_fresh(&mut self) -> Result<()> {
        info!("Entering fresh mode: dropping all indices");
        self.bulk_mode = true;
        self.connection
            .batch_execute(include_str!("pg_mode_fresh.sql"))?;
        Ok(())
    }

    fn mode_normal(&mut self) -> Result<()> {
        self.bulk_mode = false;
        self.flush_batch();
        self.flush_workers();
        info!("Entering normal mode: creating all indices");
        self.connection
            .batch_execute(include_str!("pg_mode_normal.sql"))?;
        Ok(())
    }

    // TODO: semantics against things in flight are unclear
    // Document.
    fn get_max_height(&mut self) -> Result<Option<BlockHeight>> {
        /*
                self.cached_max_height = self
                    .connection
                    .query("SELECT MAX(height) FROM blocks", &[])?
                    .iter()
                    .next()
                    .and_then(|row| row.get::<_, Option<i64>>(0))
                    .map(|u| u as u64);
        */
        self.cached_max_height = self
            .connection
            .query("SELECT height FROM blocks ORDER BY id DESC LIMIT 1", &[])?
            .iter()
            .next()
            .and_then(|row| row.get::<_, Option<i64>>(0))
            .map(|u| u as u64);

        Ok(self.cached_max_height)
    }

    fn get_hash_by_height(&mut self, height: BlockHeight) -> Result<Option<BlockHash>> {
        if let Some(max_height) = self.cached_max_height {
            if max_height < height {
                return Ok(None);
            }
        }

        // TODO: This could be done better, if we were just tracking
        // things in flight better
        self.flush_batch();
        if !self.in_flight.lock().unwrap().is_empty() {
            eprintln!("TODO: Unnecessary flush");
            self.flush_workers();
        }

        Ok(self
            .connection
            .query(
                "SELECT hash FROM blocks WHERE height = $1",
                &[&(height as i64)],
            )?
            .iter()
            .next()
            .map(|row| row.get::<_, Vec<u8>>(0))
            .map(|mut human_bytes| {
                human_bytes.reverse();
                BlockHash::from(human_bytes.as_slice())
            }))
    }

    fn reorg_at_height(&mut self, height: BlockHeight) -> Result<()> {
        info!("Reorg detected at {}H", height);
        // If we're doing reorgs, that means we have to be close to chainhead
        // this will also flush the batch and workers
        self.mode_normal();

        // Always start with removing `blocks` since that invalidates
        // all other data in case of crash
        self.connection
            .execute("REMOVE FROM blocks WHERE height >= $1", &[&(height as i64)])?;
        self.connection
            .execute("REMOVE FROM txs WHERE height >= $1", &[&(height as i64)])?;
        self.connection
            .execute("REMOVE FROM inputs WHERE height >= $1", &[&(height as i64)])?;
        self.connection.execute(
            "REMOVE FROM outputs WHERE height >= $1",
            &[&(height as i64)],
        )?;

        self.cached_max_height = None;
        Ok(())
    }

    fn insert(&mut self, info: BlockInfo) -> Result<()> {
        self.update_max_height(&info);

        self.batch_txs_total += info.block.txdata.len() as u64;
        self.batch.push(info);
        if self.batch_txs_total > 100_000 {
            self.flush_batch();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_batch();
        Ok(())
    }
}
