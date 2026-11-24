//! Inactive, transaction-local replay horizons have a fixed encoded-byte budget.
use super::{ReadInventory, Snapshot};
use crate::wire::Id;
use anyhow::Result;
use std::{
    collections::VecDeque,
    io::{self, Write},
};

const LIMIT: usize = 8 * 1024 * 1024;
#[derive(Default)]
pub(super) struct Cache(VecDeque<(Id, Box<[u8]>)>);
struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.len() > LIMIT - self.0.len() {
            return Err(io::Error::other("replay cache capacity"));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Cache {
    pub(super) fn remember(
        &mut self,
        id: Id,
        cut: &Snapshot,
        inventory: &ReadInventory,
    ) -> Result<()> {
        if cut.objects.len().saturating_mul(64) > LIMIT {
            return Ok(());
        }
        let mut bytes = Bounded(Vec::new());
        if serde_json::to_writer(&mut bytes, &(cut.replay_header()?, &cut.pages, inventory))
            .is_err()
        {
            return Ok(());
        }
        let mut total = self.0.iter().map(|(_, v)| v.len()).sum::<usize>() + bytes.0.len();
        while total > LIMIT || self.0.len() >= 8 {
            total -= self.0.pop_front().expect("nonempty replay cache").1.len();
        }
        self.0.push_back((id, bytes.0.into_boxed_slice()));
        Ok(())
    }
    pub(super) fn take(&mut self, id: Id) -> Result<Option<(Snapshot, ReadInventory)>> {
        let Some(n) = self.0.iter().position(|(key, _)| *key == id) else {
            return Ok(None);
        };
        let (_, bytes) = self.0.remove(n).expect("located horizon");
        let (header, pages, inventory): (Vec<u8>, _, _) = serde_json::from_slice(&bytes)?;
        Ok(Some((
            Snapshot::restore_replay(&header, &pages)?,
            inventory,
        )))
    }
}
