// Post-transpile workaround for a jco codegen bug (observed in jco 1.25.2 and
// still present on jco main at the time of writing).
//
// jco tracks in-flight resource borrows in a global `curResourceBorrows`
// array. Trampolines for imports on *host-implemented* resources push the
// resource itself (`curResourceBorrows.push(rsc0)`), and their cleanup
// iterates plain entries. But the cleanup form is chosen from an arbitrary
// entry of the function's resource map (`resource_map.iter().nth(0)` in
// `function_bindgen.rs`), so a stream-carrying import on a host resource can
// emit the *guest*-resource cleanup form — destructuring `{ rsc, drop }` from
// entries that are plain resources — and crash with
// `TypeError: Cannot read properties of undefined (reading 'Symbol(handle)')`.
//
// This script rewrites that cleanup into a shape-tolerant loop that handles
// both entry forms. Only this adapter's transpile step (blanket `-I async`
// over the conformance guest, see package.json) runs it — the only pipeline
// where the broken cleanup has been observed. It can be deleted once jco
// generates consistent forms; it fails when it matches nothing so that
// upgrade is a deliberate decision rather than a silent no-op.

import { readFile, writeFile } from "node:fs/promises";

// The generated cleanup appears at varying indentation; match it structurally.
const BROKEN =
  /for \(const \{ rsc, drop \} of curResourceBorrows\) \{\s*if \(rsc\[symbolRscHandle\]\) \{\s*drop\(rsc\[symbolRscHandle\]\);\s*rsc\[symbolRscHandle\] = undefined;\s*\}\s*\}/g;

const FIXED = `for (const entry of curResourceBorrows) {
  const rsc = entry.rsc ?? entry;
  if (entry.drop && rsc[symbolRscHandle]) entry.drop(rsc[symbolRscHandle]);
  rsc[symbolRscHandle] = undefined;
}`;

const path = process.argv[2];
if (!path) {
  console.error("usage: node patch-generated.mjs <generated-module.js>");
  process.exit(1);
}
const source = await readFile(path, "utf8");
let count = 0;
const patched = source.replace(BROKEN, () => {
  count += 1;
  return FIXED;
});
if (count === 0) {
  // Fail closed: with the pinned jco version the broken cleanup shape must be
  // present, so zero matches means either (a) jco was upgraded and generates
  // consistent forms now — delete this patch step from the transpile script —
  // or (b) jco's codegen reformatted the still-broken cleanup and the BROKEN
  // regex must be updated. Continuing silently would defer the failure to a
  // cryptic runtime TypeError deep inside a test run.
  console.error(
    `patch-generated: found no borrow-cleanup loop to rewrite in ${path}.\n` +
      "If the jco upgrade fixed the codegen (see the header), remove this " +
      "patch step; if the generated shape merely changed, update BROKEN.",
  );
  process.exit(1);
}
await writeFile(path, patched);
console.error(`patch-generated: rewrote ${count} borrow-cleanup loop(s) in ${path}`);
