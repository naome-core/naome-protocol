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
        let ((header, scope), pages, inventory): ((Vec<u8>, Vec<Id>), _, _) =
            serde_json::from_slice(&bytes)?;
        id.verify(&header)?;
        Ok(Some((
            Snapshot::restore_replay(&header, scope, &pages)?,
            inventory,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        profile::{PUBLIC, READ_ONLY},
        store::{Archive, ManifestEncoding},
    };

    #[test]
    fn restoring_a_historical_cut_preserves_its_selected_access_context() -> Result<()> {
        let source = Archive::memory()?;
        let evidence = b"historical observation held under a different local grant";
        let id = source.blob(evidence, Id::blob(READ_ONLY))?;
        let historical = source.snapshot("historical cut", Default::default())?;
        let historical_id = source.deposit_manifest(&historical)?;
        let published = source.snapshot_id("published cut", [historical_id].into())?;
        source.withdraw(id)?;

        let mut recipient = Archive::memory()?;
        recipient.blob(evidence, Id::blob(PUBLIC))?;
        let page = source.export_exchange_page(published, None, 64)?;
        assert!(
            recipient
                .import_pinned_exchange_page(published, &page)?
                .complete
        );
        let selected = recipient.head(published)?;
        let historical =
            recipient.linked_snapshot(&selected, historical_id, ManifestEncoding::Canonical)?;
        assert!(recipient.resolve(&historical, id, "read").is_err());

        let mut cache = Cache::default();
        cache.remember(historical_id, &historical, &ReadInventory::for_replay())?;
        let (restored, _) = cache.take(historical_id)?.expect("retained historical cut");
        assert_eq!(restored.id()?, historical_id);
        assert!(recipient.resolve(&restored, id, "read").is_err());
        assert!(recipient.resolve(&restored, id, "train").is_err());
        Ok(())
    }
}
