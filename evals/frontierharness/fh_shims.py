"""Node shims installed into task containers (Node always ships with OpenCode).

Kept free of Harbor imports so tests can execute them with a local Node.
"""

# The wrapper journals every call and refuses flags that would point slopdex at
# another workspace, index or config.
WRAPPER_JS = r"""#!__NODE__
const { spawnSync } = require('child_process');
const fs = require('fs');
const crypto = require('crypto');
const FORBIDDEN = ['--root', '--config', '--index'];
const args = process.argv.slice(2);
const id = crypto.randomBytes(8).toString('hex');
const record = (value) => { try { fs.appendFileSync(__LOG__, JSON.stringify(value) + '\n'); } catch (_) {} };
const blocked = args.some((arg) => FORBIDDEN.some((flag) => arg === flag || arg.startsWith(flag + '=')));
record({ event: 'start', id, argv: args, allowed: !blocked, time: Date.now() / 1000 });
let code;
if (blocked) {
  process.stderr.write('slopdex workspace, config, and index overrides are unavailable in this evaluation\n');
  code = 126;
} else {
  const result = spawnSync(__BIN__, [...__PREFIX__, ...args], { stdio: 'inherit' });
  code = result.status === null ? 1 : result.status;
  if (result.error) { process.stderr.write(String(result.error) + '\n'); code = 127; }
}
record({ event: 'finish', id, exit_code: code, end_time: Date.now() / 1000 });
process.exit(code);
"""

# slopdex appends --model for executables named `opencode`; the shim adds JSON
# events, prints the final answer text, and journals the nested usage.
FAQ_SHIM_JS = r"""#!__NODE__
const { spawnSync } = require('child_process');
const fs = require('fs');
let argv = process.argv.slice(2);
const split = argv.indexOf('--');
const position = split < 0 ? argv.length : split;
argv = [...argv.slice(0, position), '--format', 'json', ...argv.slice(position)];
const started = Date.now() / 1000;
const result = spawnSync(__OPENCODE__, argv, { stdio: ['inherit', 'pipe', 'inherit'], maxBuffer: 1 << 30 });
const texts = new Map();
const tokens = { input: 0, output: 0, reasoning: 0, cache_read: 0, cache_write: 0 };
let cost = 0, steps = 0, errors = 0;
for (const line of String(result.stdout || '').split('\n')) {
  if (!line.trim()) continue;
  let event;
  try { event = JSON.parse(line); } catch (_) { continue; }
  const part = event.part || {};
  if (event.type === 'text') {
    const key = part.messageID || 'unknown';
    texts.set(key, (texts.get(key) || '') + (part.text || ''));
  } else if (event.type === 'step_finish') {
    steps += 1;
    cost += part.cost || 0;
    const usage = part.tokens || {};
    for (const name of ['input', 'output', 'reasoning']) tokens[name] += usage[name] || 0;
    tokens.cache_read += (usage.cache || {}).read || 0;
    tokens.cache_write += (usage.cache || {}).write || 0;
  } else if (event.type === 'error') {
    errors += 1;
  }
}
const answer = texts.size ? [...texts.values()].pop() : '';
const code = result.status === null ? 1 : result.status;
try {
  fs.appendFileSync(__LOG__, JSON.stringify({ exit_code: code, seconds: Date.now() / 1000 - started, tokens,
    cost_usd: steps ? cost : null, steps, errors }) + '\n');
} catch (_) {}
process.stdout.write(answer);
process.exit(code || (answer.trim() ? 0 : 1));
"""


def render_js(template: str, **values: str) -> str:
    for key, value in values.items():
        template = template.replace(f"__{key}__", value)
    return template
