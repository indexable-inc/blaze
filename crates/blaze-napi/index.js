// The loader of blaze's Node-API addon (docs/client.md): finds the built `.node` file and
// wraps its three functions (connect, call, close) in the Client that index.d.ts types.
// Options and answers cross the addon as JSON text, the API's one rendering; this file only
// turns them into values and back. There is no other binding and no fallback: when the addon
// is not found, this throws, naming every place it looked.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

function library() {
  const name = process.platform === 'darwin' ? 'libblaze_napi.dylib' : 'libblaze_napi.so';
  const repo = path.resolve(__dirname, '..', '..');
  const looked = [];
  if (process.env.BLAZE_NAPI) looked.push(process.env.BLAZE_NAPI);
  looked.push(path.join(__dirname, 'blaze.node'));
  for (const profile of ['release', 'debug']) {
    looked.push(path.join(repo, 'target', profile, name));
  }
  const found = looked.find((it) => fs.existsSync(it));
  if (!found) {
    throw new Error(
      `blaze-napi: the addon is not built. Looked at: ${looked.join(', ')}. ` +
        'Build crates/blaze-napi with blaze and copy its library to crates/blaze-napi/blaze.node, ' +
        'or name the file in BLAZE_NAPI (docs/client.md).',
    );
  }
  return found;
}

function load() {
  const file = library();
  // A host loads an addon by its `.node` name; a library under its built name is loaded
  // with process.dlopen, which takes any name.
  if (file.endsWith('.node')) return require(file);
  const loaded = { exports: {} };
  process.dlopen(loaded, file);
  return loaded.exports;
}

const addon = load();

class Client {
  #handle;

  constructor(handle) {
    this.#handle = handle;
  }

  async #call(method, options, onEvent) {
    const told = onEvent ? (line) => onEvent(JSON.parse(line).body) : undefined;
    return JSON.parse(await addon.call(this.#handle, method, JSON.stringify(options), told));
  }

  build(options, onEvent) {
    return this.#call('build', options, onEvent);
  }

  check(options, onEvent) {
    return this.#call('check', options, onEvent);
  }

  test(options, onEvent) {
    return this.#call('test', options, onEvent);
  }

  why(options) {
    return this.#call('why', options);
  }

  status(options) {
    return this.#call('status', options);
  }

  request(options, onEvent) {
    return this.#call('request', options, onEvent);
  }

  close() {
    return addon.close(this.#handle);
  }
}

async function connect(options) {
  return new Client(await addon.connect(JSON.stringify(options)));
}

module.exports = { connect, Client };
