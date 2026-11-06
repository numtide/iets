// Runs `iets.wasm` as a browser does: no directories and no environment.
// The engine ending the instance gives exit status 99.
import { readFileSync } from "node:fs";
import { WASI } from "node:wasi";

const [wasm, ...args] = process.argv.slice(2);
const wasi = new WASI({ version: "preview1", args: ["iets", ...args], returnOnExit: true });
const module = await WebAssembly.compile(readFileSync(wasm));
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
let status;
try {
  status = wasi.start(instance);
} catch (e) {
  console.error(`${e}`);
  status = 99;
}
process.exitCode = status;
