export interface GitHubUser {
  id: number;
  login: string;
  email: string | null;
  avatar_url: string;
}

/** Exchange an OAuth authorization code for an access token. */
export async function exchangeCodeForToken(
  code: string,
  clientId: string,
  clientSecret: string,
  redirectUri: string,
): Promise<string> {
  const resp = await fetch("https://github.com/login/oauth/access_token", {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      client_id: clientId,
      client_secret: clientSecret,
      code,
      redirect_uri: redirectUri,
    }),
  });

  const data = (await resp.json()) as {
    access_token?: string;
    error?: string;
    error_description?: string;
  };

  if (!data.access_token) {
    throw new Error(
      data.error_description ?? data.error ?? "GitHub token exchange failed",
    );
  }

  return data.access_token;
}

/** Fetch the authenticated GitHub user profile. */
export async function fetchGitHubUser(
  accessToken: string,
): Promise<GitHubUser> {
  const resp = await fetch("https://api.github.com/user", {
    headers: {
      Authorization: `Bearer ${accessToken}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "recon-api/1.0",
    },
  });

  if (!resp.ok) {
    throw new Error(`GitHub API error: ${resp.status} ${resp.statusText}`);
  }

  return (await resp.json()) as GitHubUser;
}
