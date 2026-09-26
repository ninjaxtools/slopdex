import { FakeEmbeddingProvider } from "../helpers.js";

export class CountingEmbeddingProvider extends FakeEmbeddingProvider {
  public documentCount = 0;

  public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
    this.documentCount += inputs.length;
    return await super.embedDocuments(inputs);
  }
}
