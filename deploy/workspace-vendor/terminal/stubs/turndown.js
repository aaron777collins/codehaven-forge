// html-to-markdown is not offered in the workspace terminal, so turndown is not shipped.
export default class TurndownService {
  constructor() {
    throw new Error("html-to-markdown is not available in this terminal");
  }
}
