use super::*;
use common_failures::prelude::*;

#[derive(Default)]
pub struct MemDataStore {
    blocks: BTreeMap<BlockHeight, Block>,
    block_hashes: BTreeMap<BlockHeight, BlockHash>,
}

impl DataStore for MemDataStore {
    fn wipe_to_height(&mut self, _height: u64) -> Result<()> {
        Ok(())
    }

    fn get_hash_by_height(&mut self, height: BlockHeight) -> Result<Option<BlockHash>> {
        Ok(self.block_hashes.get(&height).cloned())
    }

    fn insert(&mut self, info: BlockInfo) -> Result<()> {
        let parsed = super::parse_node_block(&info)?;
        self.blocks.insert(info.height, parsed.block);
        Ok(())
    }

    fn get_max_height(&mut self) -> Result<Option<BlockHeight>> {
        Ok(self.blocks.keys().next_back().cloned())
    }
}
