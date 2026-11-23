// gzip, gunzip and zcat are not offered in the workspace terminal, so no zlib is shipped.
const no = () => {
  throw new Error("gzip is not available in this terminal");
};
export const gzipSync = no;
export const gunzipSync = no;
export const constants = {};
