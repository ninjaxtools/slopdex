import type { CallableKind, SupportedLanguage } from "../src/types.js";

// Cases adapted from ../treesitter-index/src/indexer/{python,typescript,rust,go,java}.rs.
// Keep inputs local: normal tests do not require that repository or its executable.
export const parityFixtures: Array<{
  language: SupportedLanguage;
  extension: string;
  source: string;
  expected: Array<[string, CallableKind, string]>;
  referenceSignatures: string[];
}> = [
  {
    language: "python", extension: "py",
    source: `class Repo[T](Base, Protocol):
    @classmethod
    async def connect(cls, url: str) -> None:
        """Connect to the repository."""
        await open(url)
@registered('worker')
async def process[T](data: list[T]) -> dict[str, int]:
    return {'size': len(data)}
`,
    expected: [
      ["Repo.connect", "method", "async def connect(cls, url: str) -> None:"],
      ["process", "function", "async def process[T](data: list[T]) -> dict[str, int]:"],
    ],
    referenceSignatures: ["async connect(cls, url: str) -> None", "async process[T](data: list[T]) -> dict[str, int]"],
  },
  {
    language: "javascript", extension: "js",
    source: `export const render = async value => value;
export function* values(items) { yield* items; }
export class View { field = () => 1; draw(target) { target.paint(); } }
function constructor() { return {}; }
`,
    expected: [
      ["render", "function", "async render(value)"], ["values", "generator", "*values(items)"],
      ["View.field", "method", "field()"], ["View.draw", "method", "draw(target)"],
      ["constructor", "function", "constructor()"],
    ],
    referenceSignatures: ["render = async value =>", "*values(items)", "draw(target)", "constructor()"],
  },
  {
    language: "jsx", extension: "jsx",
    source: `export function Screen(props) { return <main>{props.title}</main>; }
export const render = props => <Screen {...props} />;
`,
    expected: [["Screen", "function", "Screen(props)"], ["render", "function", "render(props)"]],
    referenceSignatures: ["Screen(props)", "render = props =>"],
  },
  {
    language: "typescript", extension: "ts",
    source: `export namespace API {
    export abstract class Base {
        abstract skip(): void;
        run<T>(value: T): T { return value; }
        handler = async value => value;
        async *values<T>(items: T[]): AsyncGenerator<T> { yield* items; }
    }
}
export async function load<T>(value: T): Promise<T> { return value; }
declare function external(): void;
`,
    expected: [
      ["API.Base.run", "method", "run<T>(value: T): T"],
      ["API.Base.handler", "method", "async handler(value)"],
      ["API.Base.values", "generator", "async *values<T>(items: T[]): AsyncGenerator<T>"],
      ["load", "function", "async load<T>(value: T): Promise<T>"],
    ],
    // The reference emits module headers rather than recursively listing members.
    referenceSignatures: ["export namespace API", "async load<T>(value: T): Promise<T>"],
  },
  {
    language: "tsx", extension: "tsx",
    source: `export abstract class Base {
    abstract skip(): void;
    render<T>(value: T): JSX.Element { return <main>{String(value)}</main>; }
}
export const View = <T,>(value: T): JSX.Element => <main>{String(value)}</main>;
`,
    expected: [["Base.render", "method", "render<T>(value: T): JSX.Element"], ["View", "function", "View<T,>(value: T): JSX.Element"]],
    referenceSignatures: ["export abstract Base", "render<T>(value: T): JSX.Element", "View = <T,>(value: T): JSX.Element =>"],
  },
  {
    language: "rust", extension: "rs",
    source: `pub async fn load<T>(value: T) -> Result<T> where T: Send { todo!() }
trait Store {
    type Item;
    fn load(&self) -> Self::Item;
    fn skip(&self) {}
}
impl Store for Cache {
    type Item = Vec<u8>;
    fn load(&self) -> Self::Item { todo!() }
    fn skip(&self) {}
}
extern "C" { fn foreign_read(buffer: *mut u8) -> i32; }
`,
    expected: [
      ["load", "function", "pub async fn load<T>(value: T) -> Result<T> where T: Send"],
      ["Store.skip", "method", "fn skip(&self)"],
      ["<Cache as Store>.load", "method", "fn load(&self) -> Self::Item"],
      ["<Cache as Store>.skip", "method", "fn skip(&self)"],
    ],
    referenceSignatures: ["pub async load<T>(value: T) -> Result<T> where T: Send", "load(&self) -> Self::Item", "skip(&self)"],
  },
  {
    language: "go", extension: "go",
    source: `package api
type Point struct { X, Y int }
type Reader interface { Read(p []byte) (int, error) }
func Load[T any](value T) error { return nil }
func (p *Point) Distance() float64 { return 0 }
func external()
`,
    expected: [["Load", "function", "func Load[T any](value T) error"], ["Point.Distance", "method", "func (p *Point) Distance() float64"]],
    referenceSignatures: ["Load[T any](value T) error", "(p *Point) Distance() float64"],
  },
  {
    language: "java", extension: "java",
    source: `public class Service<T> {
    public Service(String name) throws IOException { this.name = name; }
    @Override public <R> R convert(T value) throws IOException { return work(value); }
    public native void external();
}
public record Point(int x, int y) {
    public Point { validate(x, y); }
    public Point(int x) { this(x, 0); }
    public int sum() { return x + y; }
}
enum Direction {
    UP(1), DOWN(-1);
    Direction(int step) { this.step = step; }
    public int delta() { return step; }
}
`,
    expected: [
      ["Service.Service", "constructor", "public Service(String name) throws IOException"],
      ["Service.convert", "method", "@Override public <R> R convert(T value) throws IOException"],
      ["Point.Point", "constructor", "public Point"], ["Point.Point", "constructor", "public Point(int x)"],
      ["Point.sum", "method", "public int sum()"], ["Direction.Direction", "constructor", "Direction(int step)"],
      ["Direction.delta", "method", "public int delta()"],
    ],
    referenceSignatures: ["public Service(String name) throws IOException", "@Override public <R> R convert(T value) throws IOException",
      "public Point(int x)", "public int sum()", "Direction(int step)", "public int delta()"],
  },
];
