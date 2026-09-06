import { copyFile, mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";

const source = path.resolve(import.meta.dirname, "..", ".agents", "skills", "slopdex", "SKILL.md");
const destinationDirectory = path.join(homedir(), ".config", "opencode", "skills", "slopdex");
const destination = path.join(destinationDirectory, "SKILL.md");

await mkdir(destinationDirectory, { recursive: true });
await copyFile(source, destination);
process.stdout.write(`Installed Slopdex skill to ${destination}\n`);
