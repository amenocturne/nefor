import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
const roots: string[] = [];
afterEach(() => { for (const root of roots.splice(0)) rmSync(root, {recursive:true, force:true}); });
function fixture(os="Linux") {
  mkdirSync("tmp", {recursive:true});
  const root=resolve(mkdtempSync("tmp/prerequisite-test.")); roots.push(root);
  const bin=`${root}/bin`, lua=`${root}/lua`, ssl=`${root}/ssl`;
  for(const dir of [bin, `${lua}/bin`,`${lua}/include/lua5.4`,`${lua}/lib`,`${ssl}/include/openssl`,`${ssl}/lib`,`${root}/tree`]) mkdirSync(dir,{recursive:true});
  const exe=(path:string, body:string)=>writeFileSync(path,`#!/bin/bash\n${body}\n`,{mode:0o755});
  exe(`${lua}/bin/lua5.4`, "printf 'Lua 5.4'"); exe(`${lua}/bin/lua`,"printf 'Lua 5.5'");
  writeFileSync(`${lua}/include/lua5.4/lua.h`, '#define LUA_VERSION_NUM 504\n');
  writeFileSync(`${ssl}/include/openssl/opensslv.h`, '#define OPENSSL_VERSION_MAJOR 3\n');
  exe(`${bin}/uname`, `echo ${os}`);
  for(const cmd of ['m4','luarocks','cargo','rustc','just','otool','install_name_tool','codesign','realpath']) exe(`${bin}/${cmd}`,':');
  exe(`${bin}/cc`, 'while [ "$1" != -o ]; do shift; done; printf "#!/bin/bash\\nexit 0\\n" > "$2"; chmod +x "$2"');
  exe(`${bin}/brew`, `case "$2" in lua@5.4) echo '${lua}';; openssl@3) echo '${ssl}';; esac`);
  exe(`${bin}/pkg-config`, `case "$*" in '--exists '*) exit 0;; '--variable=prefix lua5.4') echo '${lua}';; '--variable=libdir lua5.4') echo '${lua}/lib';; '--variable=prefix openssl') echo '${ssl}';; '--variable=includedir openssl') echo '${ssl}/include';; '--variable=libdir openssl') echo '${ssl}/lib';; esac`);
  exe(`${bin}/make`,"echo 'GNU Make'");exe(`${bin}/m4`,"echo 'GNU M4'");
  exe(`${bin}/curl`, `echo "$*" >> '${root}/curl.log'; while [ "$1" != -o ]; do shift; done; printf payload > "$2"`);
  exe(`${bin}/luarocks`, `echo "HOME=$HOME CONFIG=$LUAROCKS_CONFIG SYS=$LUAROCKS_SYSCONFDIR CWD=$PWD" >> '${root}/env.log'; echo "$*" >> '${root}/rocks.log'; if [[ "$*" = *make* ]] && [ "\${FAIL_CLOSURE:-}" = 1 ]; then exit 1; fi`);
  for(const path of [`${lua}/lib/liblua5.4.so`,`${ssl}/lib/libssl.so`,`${ssl}/lib/libcrypto.so`]) writeFileSync(path,'');
  for(const path of [`${lua}/include/lua5.4/lauxlib.h`,`${ssl}/include/openssl/ssl.h`,`${ssl}/include/openssl/crypto.h`]) writeFileSync(path,'');
  const env={...process.env, PATH:`${bin}:${process.env.PATH}`, SOURCE_PREREQUISITES_TMP:`${root}/scratch`, LUA_DEPENDENCIES_LUA_DIR:lua, LUA_DEPENDENCIES_OPENSSL_DIR:ssl};
  const run=(script:string, more:Record<string,string|undefined>={})=>Bun.spawnSync(['bash','-c',script],{env:{...env,...more}});
  const preflight=(more:Record<string,string|undefined>={})=>run('source tools/source-prerequisites.sh; source_prerequisites',more);
  writeFileSync(`${root}/tree/keep`,'unchanged');
  const digest=new Bun.CryptoHasher('sha256').update('payload').digest('hex');
  const row=`example 1-1 src ${digest} https://example.org/explicit.rock\n`;
  const prepare=(lock:string, more:Record<string,string|undefined>={})=>{writeFileSync(`${root}/lock`,lock);writeFileSync(`${root}/test.rockspec`,'');return run(`tools/prepare-lua-dependencies.sh '${root}/test.rockspec' '${root}/lock' '${root}/tree'`,more);};
  return {root,bin,lua,ssl,exe,preflight,prepare,row};
}
test('Linux selects installed 5.4 despite default 5.5, with version-aware paths',()=>{
 const f=fixture(); const r=f.preflight({LUA_DEPENDENCIES_LUA_DIR:undefined,LUA_DEPENDENCIES_OPENSSL_DIR:undefined}); expect(r.exitCode).toBe(0);expect(r.stderr.toString()).toContain('lua5.4');
});
test('simulated Darwin discovers installed keg-only Lua and OpenSSL without overrides',()=>{
 const f=fixture('Darwin'); expect(f.preflight({LUA_DEPENDENCIES_LUA_DIR:undefined,LUA_DEPENDENCIES_OPENSSL_DIR:undefined}).exitCode).toBe(0);
});
test('invalid overrides never fall back',()=>{
 const f=fixture(); for(const key of ['LUA_DEPENDENCIES_LUA_DIR','LUA_DEPENDENCIES_OPENSSL_DIR']) { const r=f.preflight({[key]:''});expect(r.exitCode).not.toBe(0);expect(r.stderr.toString()).toContain(`invalid ${key}`); }
});
test('wrong interpreter and headers are distinct failures',()=>{
 const f=fixture(); f.exe(`${f.lua}/bin/lua5.4`,"echo 'Lua 5.5'");expect(f.preflight().stderr.toString()).toContain('interpreter');f.exe(`${f.lua}/bin/lua5.4`,"printf 'Lua 5.4'");writeFileSync(`${f.lua}/include/lua5.4/lua.h`,'#define LUA_VERSION_NUM 505');expect(f.preflight().stderr.toString()).toContain('headers');
});
test('missing OpenSSL, failing link and unsupported OS fail',()=>{
 const f=fixture();rmSync(`${f.ssl}/include/openssl/opensslv.h`);expect(f.preflight().stderr.toString()).toContain('OpenSSL headers missing');
 const g=fixture();g.exe(`${g.bin}/cc`,'exit 1');expect(g.preflight().exitCode).not.toBe(0);
 expect(fixture('FreeBSD').preflight().stderr.toString()).toContain('unsupported');
});
test('all lock validation happens before downloads and preserves old tree',()=>{
 const f=fixture();for(const lock of ['',f.row+f.row,f.row+'broken\n',f.row.replace('https://','http://'),f.row.split(' https://')[0]+'\n']) {expect(f.prepare(lock).exitCode).not.toBe(0);expect(()=>readFileSync(`${f.root}/curl.log`)).toThrow();expect(readFileSync(`${f.root}/tree/keep`,'utf8')).toBe('unchanged');}
});
test('explicit downloads, private config, local installs and closure verification',()=>{
 const f=fixture(); expect(f.prepare(f.row).exitCode).toBe(0);const curl=readFileSync(`${f.root}/curl.log`,'utf8');expect(curl).toContain('https://example.org/explicit.rock');expect(curl).toContain('--proto-redir =https');const rocks=readFileSync(`${f.root}/rocks.log`,'utf8');expect(rocks).not.toContain(' download ');expect(rocks).toContain('--deps-mode=none');expect(rocks).toContain('--only-server=');const env=readFileSync(`${f.root}/env.log`,'utf8');expect(env).toContain('/tree.prepare.');expect(env).not.toContain(`HOME=${process.env.HOME} `);
});
test('curl stderr, checksum and unsatisfied closure fail without replacing existing tree',()=>{
 const f=fixture(); f.exe(`${f.bin}/curl`,"echo 'curl: (60) SSL certificate problem' >&2; exit 60"); const r=f.prepare(f.row);expect(r.exitCode).toBe(60);expect(r.stderr.toString()).toContain('curl: (60)');expect(readFileSync(`${f.root}/tree/keep`,'utf8')).toBe('unchanged');
 const g=fixture();expect(g.prepare(g.row.replace(/[0-9a-f]{64}/,'0'.repeat(64))).stderr.toString()).toContain('checksum mismatch');expect(readFileSync(`${g.root}/tree/keep`,'utf8')).toBe('unchanged');
 const h=fixture();expect(h.prepare(h.row,{FAIL_CLOSURE:'1'}).stderr.toString()).toContain('locked dependency closure');expect(readFileSync(`${h.root}/tree/keep`,'utf8')).toBe('unchanged');
});

test('missing required programs are actionable, not shell failures',()=>{
 const f=fixture();
 const r=Bun.spawnSync(['bash','-c','source tools/source-prerequisites.sh; command() { if [ "$2" = m4 ]; then return 1; else builtin command "$@"; fi; }; source_prerequisites'],{env:{...process.env,PATH:f.bin+':'+process.env.PATH}});
 expect(r.exitCode).not.toBe(0);expect(r.stderr.toString()).toContain('source install requires m4');
});

test('missing selected-prefix libraries cannot be satisfied by another system install',()=>{
 const f=fixture();rmSync(`${f.ssl}/lib/libcrypto.so`);expect(f.preflight().stderr.toString()).toContain('crypto development library missing');
 const g=fixture();rmSync(`${g.lua}/lib/liblua5.4.so`);g.exe(`${g.bin}/cc`,'echo fixture-linux-gnu');expect(g.preflight().stderr.toString()).toContain('Lua 5.4 development library missing');
});
