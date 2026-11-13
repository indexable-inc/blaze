/** Bindings from wrangler.jsonc. */
export interface Env {
	ASSETS: Fetcher;
	/** blaze's release metadata (R2 bucket `blaze-releases`): `latest.json`, served at /latest.json. */
	RELEASES: R2Bucket;
}
