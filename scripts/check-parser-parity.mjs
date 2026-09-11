import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";

import { parseCallables } from "../src/parser/callable-parser.ts";
import { parityFixtures } from "../tests/parser-parity-fixtures.ts";

const executable = path.resolve(process.argv[2]
  ?? path.resolve(import.meta.dirname, "../../treesitter-index/target/debug/treesitter-index"));

for (const fixture of parityFixtures) {
  const reference = (format) => execFileSync(executable, ["-t", fixture.language, "--format", format], {
    input: fixture.source, encoding: "utf8",
  });
  assert.equal(JSON.parse(reference("json")).hasError, false, `${fixture.language}: reference syntax errors`);
  const skeleton = reference("skeleton");
  for (const signature of fixture.referenceSignatures) {
    assert.ok(skeleton.includes(signature), `${fixture.language}: reference missing ${signature}`);
  }
  const warnings = [];
  const functions = parseCallables(`parity.${fixture.extension}`, fixture.source, (message) => warnings.push(message));
  assert.deepEqual(warnings, [], `${fixture.language}: slopdex syntax errors`);
  assert.deepEqual(functions.map((item) => [item.qualifiedName, item.kind, item.signature]), fixture.expected);
  console.log(`${fixture.language}: ${functions.length} callables and ${fixture.referenceSignatures.length} reference signature checks passed`);
}
console.log("Checked fixtures for all 8 shared languages. The reference has no C grammar.");
