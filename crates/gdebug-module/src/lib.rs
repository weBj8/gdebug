//! gdebug native module: an attach debugger for Garry's Mod Lua (doc/SPEC.md).
//!
//! Inert unless `GDEBUG_SERVER_ENDPOINT_FILE` / `GDEBUG_CLIENT_ENDPOINT_FILE` names the private
//! endpoint descriptor path for this realm. No debug hook is installed until a controller
//! attaches with explicit hook-replacement consent.

pub mod dbg;
pub mod net;

#[cfg(any(feature = "server", feature = "client"))]
mod gmod {
    use rsgdll::prelude::*;

    rsgdll::module!(name = "gdebug");

    #[rsgdll::postopen]
    fn open(lua: &Lua) -> Result<()> {
        let realm = if lua.is_server() { "server" } else { "client" };
        // SAFETY: rsgdll lifecycle callbacks run on the owner thread with a live state.
        let msg = match unsafe { crate::dbg::open(lua.raw(), realm, lua.generation()) } {
            Ok(m) | Err(m) => m,
        };
        lua.print(&msg)
    }

    #[rsgdll::preclose]
    fn close(lua: &Lua) {
        unsafe { crate::dbg::close(lua.raw()) };
    }

    #[rsgdll::hook(Think)]
    fn on_think(lua: &Lua) -> Result<()> {
        unsafe { crate::dbg::think(lua.raw()) };
        Ok(())
    }
}
