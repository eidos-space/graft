import { diff3Merge } from "node-diff3";

export function prepareTextMerge(base: string, current: string, incoming: string) {
  const ending = current.includes("\r\n") ? "\r\n" : "\n";
  const lines = (text: string) => text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
  let conflicts = 0;
  const chunks: string[] = [];
  for (const block of diff3Merge(lines(current), lines(base), lines(incoming))) {
    if (block.ok) chunks.push(...block.ok);
    else if (block.conflict) {
      conflicts++;
      const newline = () => {
        if (chunks.length && !chunks.at(-1)!.endsWith("\n")) chunks.push(ending);
      };
      newline();
      chunks.push(`<<<<<<< Current${ending}`, ...block.conflict.a);
      newline();
      chunks.push(`=======${ending}`, ...block.conflict.b);
      newline();
      chunks.push(`>>>>>>> Incoming${ending}`);
    }
  }
  return {
    contents: chunks.join(""),
    conflicts,
    trimFinalNewline: conflicts > 0 && !current.endsWith("\n") && !incoming.endsWith("\n"),
  };
}
