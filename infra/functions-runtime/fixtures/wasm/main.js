// add.wasm exports add(i32, i32) -> i32; it arrives as a compiled WebAssembly.Module.
import add from "./add.wasm";

export default {
  async fetch() {
    const instance = await WebAssembly.instantiate(add);
    return new Response(`2+3=${instance.exports.add(2, 3)}`);
  },
};
