// Echoes what user code can see: its request headers and its env.
export default {
  fetch(request, env) {
    return Response.json({
      headers: Object.fromEntries(request.headers),
      envKeys: Object.keys(env).sort(),
      env,
    });
  },
};
