#!/usr/bin/env python3
"""Validate and summarize the captured topology; no production access."""
from collections import Counter, deque
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def component_sizes(count, edges, included):
    adjacency = [[] for _ in range(count)]
    for source, target in edges:
        if source in included and target in included:
            adjacency[source].append(target)
            adjacency[target].append(source)
    seen = set()
    sizes = []
    for start in sorted(included):
        if start in seen:
            continue
        pending = [start]
        seen.add(start)
        size = 0
        while pending:
            node = pending.pop()
            size += 1
            for neighbor in adjacency[node]:
                if neighbor not in seen:
                    seen.add(neighbor)
                    pending.append(neighbor)
        sizes.append(size)
    return sorted(sizes, reverse=True)


def main():
    fixture = json.loads((ROOT / 'hyphae/benches/fixtures/scene_topology.json').read_text())
    nodes = fixture['nodes']
    included = {i for i, node in enumerate(nodes) if node['role'] != 'annotation'}
    wires = fixture['edges']
    refs = [(a, b) for a, b in fixture['scene_output_links'] if a is not None]
    edges = wires + refs
    assert all(0 <= source < len(nodes) and 0 <= target < len(nodes) for source, target in edges)
    degree = [0] * len(nodes)
    children = [[] for _ in nodes]
    for source, target in edges:
        degree[target] += 1
        children[source].append(target)
    queue = deque(i for i, count in enumerate(degree) if count == 0)
    depths = [0] * len(nodes)
    visited = 0
    while queue:
        source = queue.popleft()
        visited += 1
        for target in children[source]:
            depths[target] = max(depths[target], depths[source] + 1)
            degree[target] -= 1
            if degree[target] == 0:
                queue.append(target)
    assert visited == len(nodes), 'captured graph is cyclic'
    controls = []
    for group in fixture['shared_groups']:
        assert all(index in included for index in group['nodes'])
        if group['kind'] == 'control-ref-id':
            controls.extend(zip(group['nodes'], group['nodes'][1:]))
    sizes = component_sizes(len(nodes), edges, included)
    with_controls = component_sizes(len(nodes), edges + controls, included)
    result = {
        'authored_nodes': len(nodes), 'computational_nodes': len(included),
        'explicit_edges': len(wires), 'resolved_scene_output_links': len(refs),
        'external_scene_output_links': sum(a is None for a, _ in fixture['scene_output_links']),
        'longest_explicit_dependency_path': max(depths),
        'node_kinds': dict(sorted(Counter(node['kind'] for node in nodes).items())),
        'scene_sizes_descending': sorted(Counter(node['scene'] for node in nodes).values(), reverse=True),
        'shared_group_counts': dict(Counter(group['kind'] for group in fixture['shared_groups'])),
        'wire_reference_components': {'count': len(sizes), 'largest': sizes[:10]},
        'with_shared_control_components': {'count': len(with_controls), 'largest': with_controls[:10]},
        'note': 'authored dependencies only; excludes hidden report/schema dependencies and effects',
    }
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
