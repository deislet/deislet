#!/usr/bin/env python3
"""
Visualize Rust project structure
Generates a graphical representation of modules, structs, and their relationships
"""

import os
import re
from pathlib import Path
import subprocess
from collections import defaultdict

def parse_rust_file(filepath):
    """Parse a Rust file and extract structural information"""
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # Extract module path from filepath
    module_path = str(filepath).replace('src/', '').replace('.rs', '').replace('/', '::')
    if module_path == 'main':
        module_path = 'crate::main'
    elif module_path == 'lib':
        module_path = 'crate'
    elif module_path.endswith('::mod'):
        module_path = module_path[:-5]
    
    info = {
        'path': module_path,
        'structs': [],
        'enums': [],
        'traits': [],
        'functions': [],
        'imports': [],
        'modules': []
    }
    
    # Extract structs
    struct_pattern = r'pub\s+struct\s+(\w+)'
    info['structs'] = re.findall(struct_pattern, content)
    
    # Extract enums
    enum_pattern = r'pub\s+enum\s+(\w+)'
    info['enums'] = re.findall(enum_pattern, content)
    
    # Extract traits
    trait_pattern = r'pub\s+trait\s+(\w+)'
    info['traits'] = re.findall(trait_pattern, content)
    
    # Extract public functions
    fn_pattern = r'pub\s+(?:async\s+)?fn\s+(\w+)'
    info['functions'] = re.findall(fn_pattern, content)
    
    # Extract module declarations
    mod_pattern = r'pub\s+mod\s+(\w+);'
    info['modules'] = re.findall(mod_pattern, content)
    
    # Extract use statements
    use_pattern = r'use\s+([\w:]+)'
    info['imports'] = re.findall(use_pattern, content)
    
    return info

def generate_dot_graph(project_info):
    """Generate DOT graph from project information"""
    dot = ['digraph RustProject {']
    dot.append('  rankdir=LR;')
    dot.append('  node [shape=box, style=rounded];')
    dot.append('  edge [color=gray];')
    dot.append('')
    
    # Define colors for different types
    colors = {
        'module': '#E8F5E9',
        'struct': '#E3F2FD',
        'enum': '#FFF3E0',
        'trait': '#F3E5F5',
        'function': '#FAFAFA'
    }
    
    # Create subgraphs for each module
    for filepath, info in project_info.items():
        module_name = info['path'].replace('::', '_')
        
        dot.append(f'  subgraph cluster_{module_name} {{')
        dot.append(f'    label="{info["path"]}";')
        dot.append('    style=filled;')
        dot.append('    color=lightgrey;')
        dot.append('    node [style=filled];')
        
        # Add structs
        for struct_name in info['structs']:
            node_id = f'{module_name}_{struct_name}'
            dot.append(f'    {node_id} [label="{struct_name}\\n[struct]", fillcolor="{colors["struct"]}"];')
        
        # Add enums
        for enum_name in info['enums']:
            node_id = f'{module_name}_{enum_name}'
            dot.append(f'    {node_id} [label="{enum_name}\\n[enum]", fillcolor="{colors["enum"]}"];')
        
        # Add traits
        for trait_name in info['traits']:
            node_id = f'{module_name}_{trait_name}'
            dot.append(f'    {node_id} [label="{trait_name}\\n[trait]", fillcolor="{colors["trait"]}"];')
        
        dot.append('  }')
        dot.append('')
    
    # Add module relationships
    for filepath, info in project_info.items():
        parent_module = info['path'].replace('::', '_')
        for module in info['modules']:
            child_module = f"{info['path']}::{module}".replace('::', '_')
            if child_module in [i['path'].replace('::', '_') for i in project_info.values()]:
                dot.append(f'  {parent_module}_mod [label="mod", shape=ellipse];')
                dot.append(f'  {parent_module}_mod -> {child_module}_mod [style=dashed];')
    
    dot.append('}')
    return '\n'.join(dot)

def generate_mermaid_diagram(project_info):
    """Generate Mermaid diagram from project information"""
    mermaid = ['graph TB']
    mermaid.append('  classDef structStyle fill:#E3F2FD,stroke:#1976D2,stroke-width:2px;')
    mermaid.append('  classDef enumStyle fill:#FFF3E0,stroke:#F57C00,stroke-width:2px;')
    mermaid.append('  classDef traitStyle fill:#F3E5F5,stroke:#7B1FA2,stroke-width:2px;')
    mermaid.append('  classDef moduleStyle fill:#E8F5E9,stroke:#388E3C,stroke-width:2px;')
    mermaid.append('')
    
    # Create nodes for each module and its contents
    for filepath, info in project_info.items():
        module_name = info['path'].replace('::', '_')
        
        # Add module node
        mermaid.append(f'  {module_name}["{info["path"]}"]:::moduleStyle')
        
        # Add structs
        for struct_name in info['structs']:
            node_id = f'{module_name}_{struct_name}'
            mermaid.append(f'  {node_id}["{struct_name}<br/>[struct]"]:::structStyle')
            mermaid.append(f'  {module_name} --> {node_id}')
        
        # Add enums
        for enum_name in info['enums']:
            node_id = f'{module_name}_{enum_name}'
            mermaid.append(f'  {node_id}["{enum_name}<br/>[enum]"]:::enumStyle')
            mermaid.append(f'  {module_name} --> {node_id}')
        
        # Add traits
        for trait_name in info['traits']:
            node_id = f'{module_name}_{trait_name}'
            mermaid.append(f'  {node_id}["{trait_name}<br/>[trait]"]:::traitStyle')
            mermaid.append(f'  {module_name} --> {node_id}')
    
    return '\n'.join(mermaid)

def main():
    """Main function to analyze Rust project structure"""
    src_dir = Path('src')
    
    # Find all Rust files
    rust_files = list(src_dir.rglob('*.rs'))
    
    # Parse all files
    project_info = {}
    for filepath in rust_files:
        info = parse_rust_file(filepath)
        project_info[str(filepath)] = info
    
    # Generate DOT graph
    dot_graph = generate_dot_graph(project_info)
    with open('project_structure.dot', 'w') as f:
        f.write(dot_graph)
    
    # Generate Mermaid diagram
    mermaid_diagram = generate_mermaid_diagram(project_info)
    with open('project_structure.mmd', 'w') as f:
        f.write(mermaid_diagram)
    
    # Try to generate PNG if graphviz is installed
    try:
        subprocess.run(['dot', '-Tpng', 'project_structure.dot', '-o', 'project_structure.png'], check=True)
        print("✓ Generated project_structure.png")
    except (subprocess.CalledProcessError, FileNotFoundError):
        print("! Graphviz not installed. Install it to generate PNG:")
        print("  sudo apt-get install graphviz")
    
    print("✓ Generated project_structure.dot (Graphviz format)")
    print("✓ Generated project_structure.mmd (Mermaid format)")
    
    # Print summary
    print("\nProject Structure Summary:")
    total_structs = sum(len(info['structs']) for info in project_info.values())
    total_enums = sum(len(info['enums']) for info in project_info.values())
    total_traits = sum(len(info['traits']) for info in project_info.values())
    total_modules = len(project_info)
    
    print(f"  Modules: {total_modules}")
    print(f"  Structs: {total_structs}")
    print(f"  Enums: {total_enums}")
    print(f"  Traits: {total_traits}")

if __name__ == '__main__':
    main()