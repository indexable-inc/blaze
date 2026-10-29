/** Bindings and vars from wrangler.jsonc, plus the secrets set with `wrangler secret put`. */
export interface Env {
	ASSETS: Fetcher;
	DB: D1Database;
	/** Who is on the site (worker/presence.ts): one object, named "site". */
	PRESENCE: DurableObjectNamespace;
	/** The deployed Worker version (`version_metadata`); its id is part of the anonymous page cache key. */
	CF_VERSION: WorkerVersionMetadata;
	/** GitHub OAuth App client id. Empty: sign-in is off (503) and the comment UI hides the button. */
	GITHUB_CLIENT_ID: string;
	/** Secret. */
	GITHUB_CLIENT_SECRET?: string;
	/** Comma-separated numeric GitHub user ids that may delete anyone's comment (ids, since a login can change hands). */
	COMMENT_ADMINS: string;
}
