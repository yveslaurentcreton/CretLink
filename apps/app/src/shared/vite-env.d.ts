// Vite's `?raw` imports, used by tests that compare a copy with its source.
declare module "*?raw" {
  const text: string;
  export default text;
}
