//! Node update signatures made on the release signer's own machine and uploaded
//! after a deploy. The controller only stores and relays them; nodes verify them
//! against the key built into them, so the administrator token alone cannot
//! make a node install anything.
use super::{Store, now};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

impl Store {
    pub fn save_node_signature(
        &self,
        os: &str,
        arch: &str,
        sha256: &str,
        manifest: &str,
        signature: &str,
    ) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO node_update_signatures (os,arch,sha256,manifest,signature,uploaded_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(os,arch,sha256) DO UPDATE SET
               manifest=excluded.manifest,signature=excluded.signature,uploaded_at=excluded.uploaded_at",
            params![os, arch, sha256, manifest, signature, now()],
        )?;
        Ok(())
    }

    /// The uploaded manifest and signature for exactly this binary.
    pub fn node_signature(
        &self,
        os: &str,
        arch: &str,
        sha256: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT manifest,signature FROM node_update_signatures
                 WHERE os=?1 AND arch=?2 AND sha256=?3",
                params![os, arch, sha256],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }
}
