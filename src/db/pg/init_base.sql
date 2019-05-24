-- fresh/base schema: a schema we populate an empty db with

-- signgle record table to keep persistent indexer state
CREATE TABLE IF NOT EXISTS indexer_state (
  bulk_mode BOOLEAN NOT NULL
);

-- events: append only
-- you can follow them one by one,
-- to follow blockchain state
-- canceling protocol is used
-- https://github.com/dpc/rust-bitcoin-indexer/wiki/How-to-interact-with-a-blockchain#canceling-protocol
CREATE TABLE IF NOT EXISTS event (
  id BIGSERIAL NOT NULL UNIQUE PRIMARY KEY,
  block_hash_id BYTEA NOT NULL,
  revert BOOLEAN NOT NULL DEFAULT FALSE
);

-- blocks: insert only
CREATE TABLE IF NOT EXISTS block (
  hash_id BYTEA NOT NULL UNIQUE PRIMARY KEY, -- the hash is split in two to save when referencing in other columns
  hash_rest BYTEA NOT NULL,
  height INT NOT NULL,
  prev_hash_id BYTEA NOT NULL,
  merkle_root BYTEA NOT NULL,
  time BIGINT NOT NULL,
  extinct BOOLEAN NOT NULL DEFAULT FALSE -- this is the only mutable column in this table (and maybe in the whole database)
);

-- We always want these two, as a lot of logic is based
-- on `block` table, and it's the smallest table overall,
-- so it doesn't matter that much (perforamnce wise)
CREATE INDEX IF NOT EXISTS block_height ON block (height);
CREATE UNIQUE INDEX IF NOT EXISTS block_height_for_not_extinct ON block (height) WHERE extinct = false;

-- txs: insert only
CREATE TABLE IF NOT EXISTS tx (
  hash_id BYTEA NOT NULL,
  hash_rest BYTEA NOT NULL,
  size INT NOT NULL,
  weight INT NOT NULL,
  fee BIGINT NOT NULL,
  locktime BIGINT NOT NULL,
  coinbase BOOLEAN NOT NULL
);


-- block -> tx: insert only
-- mapping between blocks and txes they include
CREATE TABLE IF NOT EXISTS block_tx (
  block_hash_id BYTEA NOT NULL,
  tx_hash_id BYTEA NOT NULL
);

-- outputs: insert only
CREATE TABLE IF NOT EXISTS output (
  tx_hash_id BYTEA NOT NULL,
  tx_idx INT NOT NULL,
  value BIGINT NOT NULL,
  address TEXT
);

-- input: insert only
CREATE TABLE IF NOT EXISTS input (
  output_tx_hash_id BYTEA NOT NULL, -- output id this tx input spends
  output_tx_idx INT NOT NULL,
  tx_hash_id BYTEA NOT NULL, -- tx id this input is from
  has_witness BOOLEAN NOT NULL
);

-- mempool: insert only
-- when was this tx first seen in the mempool
CREATE TABLE IF NOT EXISTS mempool (
  tx_hash_id BYTEA NOT NULL,
  ts TIMESTAMP NOT NULL DEFAULT (timezone('utc', now()))
);


-- TODO: move to "normal mode schema"
-- these views make sense only in `normal` mode (since they rely on indices)
-- but right now it's easier to verify their syntax on start
CREATE OR REPLACE VIEW address_balance AS
  SELECT address, SUM(
    CASE WHEN input.output_tx_hash_id IS NULL THEN value ELSE 0 END
  ) AS value
  FROM output
  JOIN tx AS output_tx ON output.tx_hash_id = output_tx.hash_id
  JOIN block_tx AS output_block_tx ON output_tx.hash_id = output_block_tx.tx_hash_id
  JOIN block AS output_block ON output_block_tx.block_hash_id = output_block.hash_id
  LEFT JOIN input
    JOIN tx AS input_tx ON input.tx_hash_id = input_tx.hash_id
    JOIN block_tx AS input_block_tx ON input_tx.hash_id = input_block_tx.tx_hash_id
    JOIN block AS input_block ON input_tx.hash_id = input_block.hash_id
  ON output.tx_hash_id = input.output_tx_hash_id AND input_block.extinct = false
  WHERE
    output_block.extinct = false
  GROUP BY
    output.address;

CREATE OR REPLACE VIEW address_balance_at_height AS
  SELECT address, block.height, SUM(
    CASE WHEN output_block.height <= block.height AND input.output_tx_hash_id IS NULL THEN output.value ELSE 0 END
  ) AS value
  FROM block
  JOIN output ON true
  JOIN tx AS output_tx ON output.tx_hash_id = output_tx.hash_id
  JOIN block_tx AS output_block_tx ON output_tx.hash_id = output_block_tx.tx_hash_id
  JOIN block AS output_block ON output_block_tx.block_hash_id = output_block.hash_id
  LEFT JOIN input
    JOIN tx AS input_tx ON input.tx_hash_id = input_tx.hash_id
    JOIN block_tx AS input_block_tx ON input_tx.hash_id = input_block_tx.tx_hash_id
    JOIN block AS input_block ON input_tx.hash_id = input_block.hash_id
  ON output.tx_hash_id = input.output_tx_hash_id AND
    input_block.extinct = false AND
    input_block.height <= block.height
  WHERE
    block.extinct = false AND
    output_block.extinct = false
  GROUP BY
    block.height,
    output.address
  ORDER BY output.address;
