// Read-only MCP execute helper. Pass the selected session ID explicitly.
// Discover the named queries/reports before using against another server version.
// Return the result to a file; do not return the unfiltered production entities.
async function captureSceneWorkload(myko, sessionId, serviceId) {
  const query = async (id, args = {}) => (await myko.query(id, args)).items;
  const server = (await query('GetConnectedServer'))[0];
  const states = await query('GetAllActiveScenes');
  const active = states.filter(a => a.sessionId === sessionId && a.lifecycleState.status === 'active')
    .map(a => a.sceneId).sort();
  const sceneIds = Object.fromEntries(active.map((id, index) => [id, index]));
  const nodes = (await query('GetAllBindingNodes')).filter(n => sceneIds[n.sceneId] !== undefined)
    .sort((a, b) => a.id.localeCompare(b.id));
  const nodeIds = Object.fromEntries(nodes.map((n, index) => [n.id, index]));
  const edges = (await query('GetAllBindingNodeConnections'))
    .filter(e => nodeIds[e.startNodeId] !== undefined && nodeIds[e.endNodeId] !== undefined)
    .map(e => [nodeIds[e.startNodeId], nodeIds[e.endNodeId]]);
  const counts = nodes.map(() => 0);
  const references = [];
  const groups = {};
  for (let offset = 0; offset < nodes.length; offset += 64) {
    const values = await query('GetBindingNodeValuesByQuery', {
      nodeId: { $in: nodes.slice(offset, offset + 64).map(n => n.id) },
    });
    for (const value of values) {
      const index = nodeIds[value.nodeId];
      counts[index]++;
      if (value.anchorId === 'publisher_node_id' && typeof value.value === 'string') {
        references.push([nodeIds[value.value] ?? null, index]);
      }
      if (['control-ref-id', 'emitter_id', 'comp_engine_ref_id', 'blend_engine_id', 'global_variable_id']
        .includes(value.anchorId) && typeof value.value === 'string') {
        const key = `${value.anchorId}:${value.value}`;
        groups[key] ??= { kind: value.anchorId, nodes: [] };
        groups[key].nodes.push(index);
      }
    }
  }
  const role = kind => ['property', 'comp-element', 'target-action'].includes(kind) ? 'sink'
    : kind === 'switch' ? 'select'
    : ['group', 'comment'].includes(kind) ? 'annotation'
    : ['clock', 'interval', 'speed', 'wave', 'value-track-sampler'].includes(kind) ? 'clock'
    : ['get-control-value', 'target-emitter', 'palette-reader', 'get-point'].includes(kind) ? 'input'
    : kind === 'constant' ? 'constant' : 'combine';
  const clients = (await query('GetAllClients')).map(c => c.id);
  const connected = (await query('GetAllInstances')).filter(i => i.serviceId === serviceId
    && i.status === 'Available' && clients.includes(i.clientId));
  const countsByType = {};
  for (const type of ['Emitters', 'Actions']) {
    countsByType[type] = (await myko.report(`Count${type}`, { serviceId })).result.count;
  }
  const engines = await query('GetCompEnginesByQuery', { serviceId });
  return {
    schema: 1,
    provenance: {
      server_version: server?.version,
      captured_at: new Date().toISOString(),
      scope: 'one selected session; active scenes only; non-transactional read-only inventory',
      connected_service_executors: connected.length,
      observed_active_scenes: active.length,
      registered_service_emitters: countsByType.Emitters,
      registered_service_actions: countsByType.Actions,
      registered_service_comp_engines: engines.length,
      referenced_comp_engines: Object.values(groups).filter(g => g.kind === 'comp_engine_ref_id').length,
      observed_pulse_rates: null,
    },
    scenes: active.length,
    nodes: nodes.map((n, index) => ({ scene: sceneIds[n.sceneId], kind: n.nodeType,
      role: role(n.nodeType), authored_fields: counts[index] })),
    edges,
    scene_output_links: references,
    shared_groups: Object.values(groups).sort((a, b) => a.nodes[0] - b.nodes[0]),
  };
}
