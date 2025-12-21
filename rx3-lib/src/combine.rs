#[macro_export]
macro_rules! combine {
    // Tuple syntax with type annotations: combine!((&cell1, &cell2), |a: &T1, b: &T2| { ... })
    (($($cell:expr),+), |$($param:ident : $param_ty:ty),+| $body:expr) => {{
        use std::sync::Arc;
        use $crate::traits::DepNode;

        let compute = Arc::new(move |$($param: $param_ty),+| $body);

        $(let $param = $cell.get();)+
        let initial = compute($(&$param),+);

        let deps: Vec<Arc<dyn DepNode>> = vec![$(Arc::new($cell.clone()) as Arc<dyn DepNode>),+];
        let derived = $crate::cell::Cell::<_, $crate::cell::CellImmutable>::derived(initial, deps);

        combine!(@capture [] [$($cell),+] [$($param),+] compute derived)
    }};

    // Tuple syntax without type annotations: combine!((&cell1, &cell2), |a, b| { ... })
    (($($cell:expr),+), |$($param:ident),+| $body:expr) => {{
        use std::sync::Arc;
        use $crate::traits::DepNode;

        let compute = Arc::new(move |$($param: &_),+| $body);

        $(let $param = $cell.get();)+
        let initial = compute($(&$param),+);

        let deps: Vec<Arc<dyn DepNode>> = vec![$(Arc::new($cell.clone()) as Arc<dyn DepNode>),+];
        let derived = $crate::cell::Cell::<_, $crate::cell::CellImmutable>::derived(initial, deps);

        combine!(@capture [] [$($cell),+] [$($param),+] compute derived)
    }};

    // Bracket syntax with type annotations (backwards compat)
    [$($cell:expr),+ => |$($param:ident : $param_ty:ty),+| $body:expr] => {{
        use std::sync::Arc;
        use $crate::traits::DepNode;

        let compute = Arc::new(move |$($param: $param_ty),+| $body);

        $(let $param = $cell.get();)+
        let initial = compute($(&$param),+);

        let deps: Vec<Arc<dyn DepNode>> = vec![$(Arc::new($cell.clone()) as Arc<dyn DepNode>),+];
        let derived = $crate::cell::Cell::<_, $crate::cell::CellImmutable>::derived(initial, deps);

        combine!(@capture [] [$($cell),+] [$($param),+] compute derived)
    }};

    // Bracket syntax without type annotations (backwards compat)
    [$($cell:expr),+ => |$($param:ident),+| $body:expr] => {{
        use std::sync::Arc;
        use $crate::traits::DepNode;

        let compute = Arc::new(move |$($param: &_),+| $body);

        $(let $param = $cell.get();)+
        let initial = compute($(&$param),+);

        let deps: Vec<Arc<dyn DepNode>> = vec![$(Arc::new($cell.clone()) as Arc<dyn DepNode>),+];
        let derived = $crate::cell::Cell::<_, $crate::cell::CellImmutable>::derived(initial, deps);

        combine!(@capture [] [$($cell),+] [$($param),+] compute derived)
    }};

    // Capture cells into variables
    (@capture [$($captured:ident)*] [$cell:expr $(,$rest_cell:expr)*] [$param:ident $(,$rest_param:ident)*] $compute:ident $new_cell:ident) => {{
        let $param = $cell.clone();
        combine!(@capture [$($captured)* $param] [$($rest_cell),*] [$($rest_param),*] $compute $new_cell)
    }};

    // Base case: all captured, now set up watchers
    (@capture [$($captured:ident)+] [] [] $compute:ident $new_cell:ident) => {{
        combine!(@watch_all [$($captured)+] [$($captured)+] $compute $new_cell)
    }};

    // Set up a watcher for each cell
    (@watch_all [$($all:ident)+] [$current:ident $($rest:ident)*] $compute:ident $derived:ident) => {{
        {
            $(let $all = $all.clone();)+
            let compute = Arc::clone(&$compute);
            let derived = $derived.clone();
            let current_cell = $current.clone();

            current_cell.watch(move |_value| {
                $(let $all = $all.get();)+
                derived.notify(compute($(&$all),+));
            });
        }
        combine!(@watch_all [$($all)+] [$($rest)*] $compute $derived)
    }};

    // Base case: all watchers set up
    (@watch_all [$($all:ident)+] [] $compute:ident $derived:ident) => {{
        $derived
    }};
}
