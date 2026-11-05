//! Stock-LuaJIT stand-in for a GMod realm, for CLI/DAP end-to-end tests: opens the debugger
//! (endpoint file from GDEBUG_SERVER_ENDPOINT_FILE) and runs `lua/host.lua` in a pumped loop
//! until killed. Not GMod qualification.
use rsgdll::__rt::sys::{self, lua::*};

const SCRIPT: &str = "local function work(n)
  local a = n * 2
  return a + 1
end
local total = 0
while true do
  total = total + work(1)
  pump()
end
";

unsafe extern "C-unwind" fn pump(l: *mut lua_State) -> i32 {
    unsafe { gdebug::dbg::think(l) };
    std::thread::sleep(std::time::Duration::from_millis(1));
    0
}

fn main() {
    let names: Vec<String> = std::env::var("RSGDLL_LUAJIT")
        .into_iter()
        .chain(["libluajit-5.1.so.2", "libluajit-5.1.so", "lua51.dll"].map(String::from))
        .collect();
    let lib = names
        .iter()
        .find_map(|n| unsafe { libloading::Library::new(n) }.ok())
        .expect("stock LuaJIT not found; set RSGDLL_LUAJIT");
    unsafe {
        sys::init_with(lib).expect("load LuaJIT");
        let a = sys::api();
        let l = (a.luaL_newstate)();
        (a.luaL_openlibs)(l);
        (a.lua_pushcclosure)(l, pump, 0);
        (a.lua_setfield)(l, LUA_GLOBALSINDEX, c"pump".as_ptr());
        eprintln!("{}", gdebug::dbg::open(l, "server", 1).expect("open"));
        if (a.luaL_loadbuffer)(
            l,
            SCRIPT.as_ptr().cast(),
            SCRIPT.len(),
            c"@lua/host.lua".as_ptr(),
        ) == LUA_OK
        {
            (a.lua_pcall)(l, 0, 0, 0);
        }
        eprintln!("host script ended");
    }
}
