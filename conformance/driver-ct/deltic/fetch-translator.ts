// Fetch (and cache) the deltic translator-shim wasm for the pinned release.
//
// deltic is a runtime linker: components are translated by a wasm build of
// its translator, shipped as a release asset so consumers need no Rust
// toolchain. This script downloads that asset once into target/deltic/,
// verifies it against the pinned sha256, and prints the cached path on
// stdout (the `conformance::run-deltic` recipe captures it and exports it
// as `DELTIC_TRANSLATOR` for the driver's deltic children).
//
// THE PIN lives here (TAG + TRANSLATOR_SHA256) and in TWO import maps:
// this directory's deno.json AND ../../../deltic-impl/deno.json (the
// module-identity constraint documented there requires the embedder URL
// to be byte-identical in both). `assertPinConsistency` checks both and
// fails loud if either drifts.
//
// Bumping: update TAG here and in BOTH deno.json files, update
// TRANSLATOR_SHA256 from the release's SHA256SUMS, delete BOTH deno.lock
// files, and re-run `deno install --allow-scripts=npm:node-datachannel`
// in this directory and in deltic-impl/ to regenerate them (commit the
// diff).

const TAG = "pre-10cc776";
const TRANSLATOR_SHA256 =
  "6d02b363785593595a789d083cda0aebb1de790726718ccf543198354fa3870c";
const ASSET = "deltic-translator-shim.wasm";

const HERE = new URL(".", import.meta.url);
const REPO_ROOT = new URL("../../../", HERE);
const CACHE_DIR = new URL(`target/deltic/${TAG}/`, REPO_ROOT);
const CACHED = new URL(ASSET, CACHE_DIR);
const RELEASE_URL =
  `https://github.com/lann/deltic/releases/download/${TAG}/${ASSET}`;

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    bytes as BufferSource,
  );
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** The one-pin-everywhere gate: every raw.githubusercontent URL in BOTH
 * sibling import maps must reference TAG. */
async function assertPinConsistency(): Promise<void> {
  const configs = [
    new URL("deno.json", HERE),
    new URL("../../../deltic-impl/deno.json", HERE),
  ];
  for (const configUrl of configs) {
    const denoJson = await Deno.readTextFile(configUrl);
    const urls = denoJson.match(/https:\/\/raw\.githubusercontent\.com[^"]+/g) ?? [];
    if (urls.length === 0) {
      throw new Error(`${configUrl.pathname}: no pinned deltic URLs found`);
    }
    for (const url of urls) {
      if (!url.includes(`/lann/deltic/${TAG}/`)) {
        throw new Error(
          `pin drift: ${configUrl.pathname} pins ${url}\n` +
            `but fetch-translator.ts pins ${TAG}`,
        );
      }
    }
  }
}

async function main() {
  await assertPinConsistency();

  try {
    const cached = await Deno.readFile(CACHED);
    if (await sha256Hex(cached) === TRANSLATOR_SHA256) {
      console.log(CACHED.pathname);
      return;
    }
    console.error(`cached ${ASSET} has a stale digest; re-fetching`);
  } catch {
    // not cached yet
  }

  console.error(`fetching ${RELEASE_URL} …`);
  const resp = await fetch(RELEASE_URL);
  if (!resp.ok) {
    throw new Error(`GET ${RELEASE_URL}: ${resp.status} ${resp.statusText}`);
  }
  const bytes = new Uint8Array(await resp.arrayBuffer());
  const got = await sha256Hex(bytes);
  if (got !== TRANSLATOR_SHA256) {
    throw new Error(
      `sha256 mismatch for ${ASSET}@${TAG}:\n  want ${TRANSLATOR_SHA256}\n  got  ${got}`,
    );
  }
  await Deno.mkdir(CACHE_DIR, { recursive: true });
  await Deno.writeFile(CACHED, bytes);
  console.log(CACHED.pathname);
}

await main();
